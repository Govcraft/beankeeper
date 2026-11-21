//! `bk report cash-flow` -- a direct-method cash flow statement.
//!
//! # Method
//!
//! 1. **Cash accounts.** Either the accounts passed with `--cash-account`
//!    (each must exist and be an asset account), or by default every asset
//!    account whose name contains `cash`, `bank`, `checking` or `savings`
//!    (case-insensitive). The bundled chart-of-accounts templates follow this
//!    naming (`Operating Cash`, `Checking Account`, `Savings Account`), whereas
//!    a code range would misclassify e.g. the personal template's
//!    `1100 Savings Account` versus the business template's
//!    `1100 Accounts Receivable`. If no account qualifies, the command fails
//!    with a validation error asking for `--cash-account`.
//! 2. **Attribution.** For every transaction dated within `--from..=--to` (in
//!    the report currency) whose cash entries have a non-zero net movement,
//!    each non-cash entry is attributed directly to its account: a credit to
//!    the counter-account is a cash inflow, a debit is an outflow. Because each
//!    transaction balances, these attributions sum exactly to the transaction's
//!    net cash movement. Transactions whose cash entries net to zero (e.g.
//!    transfers between two cash accounts) are excluded.
//! 3. **Classification** by counter-account type: revenue/expense →
//!    operating; (non-cash) asset → investing; liability/equity → financing.
//! 4. **Reconciliation.** Opening cash is the balance of the cash accounts
//!    before `--from`; closing cash is computed independently as the balance
//!    through `--to`. The report flags any difference between
//!    `opening + net change` and `closing`.

use std::collections::{BTreeMap, BTreeSet};

use beankeeper::types::{AccountCode, AccountType, Currency, Money};
use rusqlite::{Connection, params, types::Type};
use serde::{Serialize, Serializer};

use super::report_dates::DateRange;
use crate::cli::{Cli, OutputFormat};
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output::amounts::{MoneyField, format_money_grouped};
use crate::output::chart;

/// Substrings (lowercase) that mark an asset account as cash by default.
pub const DEFAULT_CASH_NAME_KEYWORDS: &[&str] = &["cash", "bank", "checking", "savings"];

/// Arguments for `bk report cash-flow`.
#[derive(clap::Args, Debug, Clone)]
pub struct CashFlowArgs {
    /// Start date (inclusive, YYYY-MM-DD). Defaults to January 1 of the end year.
    #[arg(long)]
    pub from: Option<String>,

    /// End date (inclusive, YYYY-MM-DD). Defaults to today.
    #[arg(long)]
    pub to: Option<String>,

    /// Account code to treat as cash (repeatable). Defaults to asset accounts
    /// whose name contains "cash", "bank", "checking" or "savings".
    #[arg(long = "cash-account", num_args = 1)]
    pub cash_account: Vec<String>,

    /// Currency code. Defaults to the company's only transaction currency
    /// (USD when there are no transactions).
    #[arg(long)]
    pub currency: Option<String>,
    /// Compare the full range with previous, year-ago, or YYYY-MM-DD..YYYY-MM-DD.
    #[arg(long)]
    pub compare: Option<super::report_periods::CompareArg>,
    /// Explain the change for one account code.
    #[arg(long, requires = "compare")]
    pub explain_change: Option<String>,
}

/// Cash flow section, determined by the counter-account type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Section {
    /// Revenue and expense counter-accounts.
    Operating,
    /// Non-cash asset counter-accounts.
    Investing,
    /// Liability and equity counter-accounts.
    Financing,
}

impl Section {
    const ALL: [Self; 3] = [Self::Operating, Self::Investing, Self::Financing];

    fn for_account_type(account_type: AccountType) -> Option<Self> {
        match account_type {
            AccountType::Revenue | AccountType::Expense => Some(Self::Operating),
            AccountType::Asset => Some(Self::Investing),
            AccountType::Liability | AccountType::Equity => Some(Self::Financing),
            _ => None,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Operating => "operating",
            Self::Investing => "investing",
            Self::Financing => "financing",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Operating => "Operating Activities",
            Self::Investing => "Investing Activities",
            Self::Financing => "Financing Activities",
        }
    }

    fn total_label(self) -> &'static str {
        match self {
            Self::Operating => "Net cash from operating activities",
            Self::Investing => "Net cash from investing activities",
            Self::Financing => "Net cash from financing activities",
        }
    }
}

/// A cash account included in the statement.
#[derive(Debug, Clone, Serialize)]
pub struct CashAccount {
    /// Account code.
    pub code: String,
    /// Account name.
    pub name: String,
}

/// Cash attributed to one counter-account.
#[derive(Debug, Clone)]
pub struct CashFlowLine {
    pub(crate) code: String,
    pub(crate) name: String,
    pub(crate) account_type: AccountType,
    pub(crate) amount: Money,
}

impl CashFlowLine {
    /// Code of the counter-account.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Name of the counter-account.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Type of the counter-account.
    #[must_use]
    pub const fn account_type(&self) -> AccountType {
        self.account_type
    }

    /// Net cash movement, positive for inflows.
    #[must_use]
    pub const fn amount(&self) -> Money {
        self.amount
    }
}

impl Serialize for CashFlowLine {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Line<'a> {
            code: &'a str,
            name: &'a str,
            account_type: &'static str,
            #[serde(flatten)]
            amount: MoneyField,
        }
        Line {
            code: self.code(),
            name: self.name(),
            account_type: self.account_type().code(),
            amount: MoneyField::new("amount", self.amount()),
        }
        .serialize(serializer)
    }
}

/// One section of the cash flow statement.
#[derive(Debug, Clone)]
pub struct CashFlowSection {
    pub(crate) lines: Vec<CashFlowLine>,
    pub(crate) total: Money,
}

impl CashFlowSection {
    /// Counter-accounts in account-code order.
    #[must_use]
    pub fn lines(&self) -> &[CashFlowLine] {
        &self.lines
    }

    /// Subtotal in the report currency.
    #[must_use]
    pub const fn total(&self) -> Money {
        self.total
    }
}

impl Serialize for CashFlowSection {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct SectionData<'a> {
            lines: &'a [CashFlowLine],
            #[serde(flatten)]
            total: MoneyField,
        }
        SectionData {
            lines: self.lines(),
            total: MoneyField::new("total", self.total()),
        }
        .serialize(serializer)
    }
}

/// The computed cash flow statement.
#[derive(Debug, Clone)]
pub struct CashFlowStatement {
    dates: DateRange,
    currency: Currency,
    pub(crate) cash_accounts: Vec<CashAccount>,
    pub(crate) operating: CashFlowSection,
    pub(crate) investing: CashFlowSection,
    pub(crate) financing: CashFlowSection,
    pub(crate) net_change: Money,
    pub(crate) opening_cash: Money,
    pub(crate) closing_cash: Money,
    pub(crate) difference: Money,
}

impl CashFlowStatement {
    /// Inclusive transaction dates.
    #[must_use]
    pub const fn dates(&self) -> DateRange {
        self.dates
    }

    /// Currency shared by all amounts.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// Accounts treated as cash in this report.
    #[must_use]
    pub fn cash_accounts(&self) -> &[CashAccount] {
        &self.cash_accounts
    }

    /// Lines and subtotal for one of the three cash flow sections.
    #[must_use]
    pub const fn section(&self, section: Section) -> &CashFlowSection {
        match section {
            Section::Operating => &self.operating,
            Section::Investing => &self.investing,
            Section::Financing => &self.financing,
        }
    }

    /// Sum of operating, investing and financing movement.
    #[must_use]
    pub const fn net_change(&self) -> Money {
        self.net_change
    }

    /// Balance before the first included date.
    #[must_use]
    pub const fn opening_cash(&self) -> Money {
        self.opening_cash
    }

    /// Balance through the last included date, computed independently.
    #[must_use]
    pub const fn closing_cash(&self) -> Money {
        self.closing_cash
    }

    /// Closing balance minus the opening balance and net movement.
    #[must_use]
    pub const fn difference(&self) -> Money {
        self.difference
    }

    /// Whether the opening balance plus movement equals the closing balance.
    #[must_use]
    pub const fn reconciled(&self) -> bool {
        self.difference.amount().minor_units() == 0
    }
}

impl Serialize for CashFlowStatement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Data<'a> {
            #[serde(flatten)]
            dates: DateRange,
            #[serde(serialize_with = "crate::output::json::serialize_display")]
            currency: Currency,
            cash_accounts: &'a [CashAccount],
            operating: &'a CashFlowSection,
            investing: &'a CashFlowSection,
            financing: &'a CashFlowSection,
            #[serde(flatten)]
            net_change: MoneyField,
            #[serde(flatten)]
            opening_cash: MoneyField,
            #[serde(flatten)]
            closing_cash: MoneyField,
            reconciled: bool,
            #[serde(flatten)]
            difference: MoneyField,
        }
        Data {
            dates: self.dates(),
            currency: self.currency(),
            cash_accounts: self.cash_accounts(),
            operating: self.section(Section::Operating),
            investing: self.section(Section::Investing),
            financing: self.section(Section::Financing),
            net_change: MoneyField::new("net_change", self.net_change()),
            opening_cash: MoneyField::new("opening_cash", self.opening_cash()),
            closing_cash: MoneyField::new("closing_cash", self.closing_cash()),
            reconciled: self.reconciled(),
            difference: MoneyField::new("difference", self.difference()),
        }
        .serialize(serializer)
    }
}

/// Run `bk report cash-flow`.
///
/// # Errors
///
/// Returns [`CliError::Validation`] for bad dates, an ambiguous or invalid
/// currency, or when no cash account can be determined;
/// [`CliError::NotFound`] for an unknown company or `--cash-account`.
pub fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    args: &CashFlowArgs,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    let mut output = super::report::ReportOutput::new(format, use_color, &mut stdout);
    run_to(cli, db, company, args, &mut output)
}

pub(crate) fn run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    args: &CashFlowArgs,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    let statement = compute(db, company, args)?;

    match output.format {
        OutputFormat::Table => writeln!(
            output.writer,
            "{}",
            render_table(&statement, output.use_color)
        )?,
        OutputFormat::Json => {
            let meta = cli.meta("report.cash-flow", Some(company));
            let chart = chart::requested(cli, || {
                chart::cash_flow(company, &statement, cli.output.chart_all)
            })?;
            output.write_json(meta, &statement, chart)?;
        }
        OutputFormat::Csv => write!(output.writer, "{}", render_csv(&statement)?)?,
    }

    if !cli.verbosity.quiet {
        if statement.reconciled() {
            eprintln!("[ok] cash flow statement generated");
        } else {
            eprintln!(
                "[!!] cash flow statement does not reconcile (difference: {})",
                format_money_grouped(statement.difference())
            );
        }
    }

    Ok(())
}

fn compute(db: &Db, company: &str, args: &CashFlowArgs) -> Result<CashFlowStatement, CliError> {
    let dates = DateRange::resolve(
        args.from.as_deref(),
        args.to.as_deref(),
        chrono::Local::now().date_naive(),
    )?;
    let conn = db.conn();
    db::audit::atomically(conn, || {
        let currency = super::report_currency::resolve_report_currency(
            conn,
            company,
            args.currency.as_deref(),
        )?;
        compute_scoped(
            db,
            company,
            dates,
            currency,
            args.cash_account.iter().map(String::as_str),
        )
    })
}

/// Compute a cash flow report on an already-open ledger using explicit dates
/// and currency. An empty account slice uses the default cash-name rule.
///
/// # Errors
/// Returns [`CliError`] for missing companies or accounts, non-asset cash
/// accounts, amount overflow, or database errors.
pub fn compute_cash_flow(
    db: &Db,
    company: &str,
    dates: DateRange,
    currency: Currency,
    accounts: &[AccountCode],
) -> Result<CashFlowStatement, CliError> {
    compute_scoped(
        db,
        company,
        dates,
        currency,
        accounts.iter().map(AccountCode::as_str),
    )
}

pub(super) fn compute_scoped<'a>(
    db: &Db,
    company: &str,
    dates: DateRange,
    currency: Currency,
    accounts: impl IntoIterator<Item = &'a str>,
) -> Result<CashFlowStatement, CliError> {
    let conn = db.conn();
    db::audit::atomically(conn, || {
        db::companies::get_company(conn, company)?;
        let cash_accounts = resolve_cash_accounts(conn, company, accounts)?;
        compute_for_accounts(conn, company, dates, currency, cash_accounts)
    })
}

fn is_default_cash_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    DEFAULT_CASH_NAME_KEYWORDS
        .iter()
        .any(|kw| lower.contains(kw))
}

fn read_account_type(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<AccountType> {
    let value: String = row.get(column)?;
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

/// Resolve the set of cash accounts (explicit or by the default rule).
fn resolve_cash_accounts<'a>(
    conn: &Connection,
    company: &str,
    explicit: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<CashAccount>, CliError> {
    let mut stmt = conn
        .prepare("SELECT code, name, type FROM accounts WHERE company_slug = ?1 ORDER BY code")?;
    let accounts = stmt
        .query_map(params![company], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                read_account_type(row, 2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let wanted: BTreeSet<_> = explicit.into_iter().collect();
    if wanted.is_empty() {
        let found: Vec<CashAccount> = accounts
            .into_iter()
            .filter(|(_, name, ty)| *ty == AccountType::Asset && is_default_cash_name(name))
            .map(|(code, name, _)| CashAccount { code, name })
            .collect();
        if found.is_empty() {
            return Err(CliError::Validation(
                "no cash accounts found: no asset account name contains \"cash\", \"bank\", \
                 \"checking\" or \"savings\"; pass --cash-account <code>"
                    .to_string(),
            ));
        }
        return Ok(found);
    }

    let mut found = Vec::with_capacity(wanted.len());
    for code in wanted {
        let Some((code, name, ty)) = accounts.iter().find(|(c, _, _)| c == code) else {
            return Err(CliError::NotFound(format!(
                "cash account '{code}' not found in company '{company}'"
            )));
        };
        if *ty != AccountType::Asset {
            let ty = ty.code();
            return Err(CliError::Validation(format!(
                "cash account '{code}' ({name}) is a {ty} account; cash accounts must be assets"
            )));
        }
        found.push(CashAccount {
            code: code.clone(),
            name: name.clone(),
        });
    }
    Ok(found)
}

/// One entry of an in-range transaction.
struct EntryFact {
    txn_id: i64,
    code: String,
    name: String,
    account_type: AccountType,
    /// Debit = positive, credit = negative.
    signed: i64,
}

/// Compute the statement from the database.
fn compute_for_accounts(
    conn: &Connection,
    company: &str,
    dates: DateRange,
    currency: Currency,
    cash_accounts: Vec<CashAccount>,
) -> Result<CashFlowStatement, CliError> {
    let from_sql = dates.from().to_string();
    let to_sql = dates.to().to_string();
    let cash_codes: BTreeSet<&str> = cash_accounts.iter().map(|a| a.code.as_str()).collect();

    let mut stmt = conn.prepare(
        "SELECT t.id, e.account_code, a.name, a.type, \
                CASE WHEN e.direction = 'debit' THEN e.amount ELSE -e.amount END \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id \
         JOIN accounts a ON a.company_slug = e.company_slug AND a.code = e.account_code \
         WHERE t.company_slug = ?1 AND t.currency = ?2 AND t.date >= ?3 AND t.date <= ?4 \
         ORDER BY t.id, e.id",
    )?;
    let facts = stmt
        .query_map(params![company, currency.code(), from_sql, to_sql], |row| {
            Ok(EntryFact {
                txn_id: row.get(0)?,
                code: row.get(1)?,
                name: row.get(2)?,
                account_type: read_account_type(row, 3)?,
                signed: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let lines = attribute_cash(&facts, &cash_codes)?;

    let mut operating = SectionAccumulator::default();
    let mut investing = SectionAccumulator::default();
    let mut financing = SectionAccumulator::default();
    for ((section, _), line) in lines {
        let accumulator = match section {
            Section::Operating => &mut operating,
            Section::Investing => &mut investing,
            Section::Financing => &mut financing,
        };
        accumulator.add(line, currency)?;
    }

    let net_change = operating
        .total
        .checked_add(investing.total)
        .and_then(|amount| amount.checked_add(financing.total))
        .ok_or_else(amount_overflow)?;
    let opening_cash = cash_balance(conn, company, currency, &cash_codes, false, &from_sql)?;
    let closing_cash = cash_balance(conn, company, currency, &cash_codes, true, &to_sql)?;
    let expected = opening_cash
        .checked_add(net_change)
        .ok_or_else(amount_overflow)?;
    let difference = closing_cash
        .checked_sub(expected)
        .ok_or_else(amount_overflow)?;

    let money = |minor| Money::from_minor(i128::from(minor), currency);
    Ok(CashFlowStatement {
        dates,
        currency,
        cash_accounts,
        operating: operating.finish(currency),
        investing: investing.finish(currency),
        financing: financing.finish(currency),
        net_change: money(net_change),
        opening_cash: money(opening_cash),
        closing_cash: money(closing_cash),
        difference: money(difference),
    })
}

struct AttributedLine {
    code: String,
    name: String,
    account_type: AccountType,
    amount: i64,
}

#[derive(Default)]
struct SectionAccumulator {
    lines: Vec<CashFlowLine>,
    total: i64,
}

impl SectionAccumulator {
    fn add(&mut self, line: AttributedLine, currency: Currency) -> Result<(), CliError> {
        self.total = self
            .total
            .checked_add(line.amount)
            .ok_or_else(amount_overflow)?;
        self.lines.push(CashFlowLine {
            code: line.code,
            name: line.name,
            account_type: line.account_type,
            amount: Money::from_minor(i128::from(line.amount), currency),
        });
        Ok(())
    }

    fn finish(self, currency: Currency) -> CashFlowSection {
        CashFlowSection {
            lines: self.lines,
            total: Money::from_minor(i128::from(self.total), currency),
        }
    }
}

/// Attribute each in-range transaction's net cash movement to its non-cash
/// counter-accounts, keyed by `(section, account code)`.
fn attribute_cash(
    facts: &[EntryFact],
    cash_codes: &BTreeSet<&str>,
) -> Result<BTreeMap<(Section, String), AttributedLine>, CliError> {
    // Group entries by transaction.
    let mut by_txn: BTreeMap<i64, Vec<&EntryFact>> = BTreeMap::new();
    for fact in facts {
        by_txn.entry(fact.txn_id).or_default().push(fact);
    }

    // (section, code) -> line
    let mut lines: BTreeMap<(Section, String), AttributedLine> = BTreeMap::new();
    for entries in by_txn.values() {
        let cash_net: i64 = entries
            .iter()
            .filter(|e| cash_codes.contains(e.code.as_str()))
            .try_fold(0_i64, |sum, entry| {
                sum.checked_add(entry.signed).ok_or_else(amount_overflow)
            })?;
        if cash_net == 0 {
            // No cash touched, or a cash-to-cash transfer: no cash flow.
            continue;
        }
        for entry in entries
            .iter()
            .filter(|e| !cash_codes.contains(e.code.as_str()))
        {
            let section = Section::for_account_type(entry.account_type).ok_or_else(|| {
                CliError::Validation(format!(
                    "account '{}' has unknown type '{}'",
                    entry.code, entry.account_type
                ))
            })?;
            let line = lines
                .entry((section, entry.code.clone()))
                .or_insert_with(|| AttributedLine {
                    code: entry.code.clone(),
                    name: entry.name.clone(),
                    account_type: entry.account_type,
                    amount: 0,
                });
            // A credit to the counter-account is a cash inflow.
            line.amount = line
                .amount
                .checked_sub(entry.signed)
                .ok_or_else(amount_overflow)?;
        }
    }

    Ok(lines)
}

fn amount_overflow() -> CliError {
    CliError::Validation("cash flow amount exceeds the supported minor-unit range".into())
}

/// Sum of debits minus credits on the cash accounts for transactions whose
/// date is before `date` (or on it, when `inclusive`).
fn cash_balance(
    conn: &Connection,
    company: &str,
    currency: Currency,
    cash_codes: &BTreeSet<&str>,
    inclusive: bool,
    date: &str,
) -> Result<i64, CliError> {
    let op = if inclusive { "<=" } else { "<" };
    let sql = format!(
        "SELECT COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE -e.amount END), 0) \
         FROM entries e JOIN transactions t ON t.id = e.transaction_id \
         WHERE t.company_slug = ?1 AND t.currency = ?2 AND e.account_code = ?3 AND t.date {op} ?4"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut total: i64 = 0;
    for code in cash_codes {
        let bal: i64 = stmt.query_row(params![company, currency.code(), code, date], |row| {
            row.get(0)
        })?;
        total = total.checked_add(bal).ok_or_else(amount_overflow)?;
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Wrap `text` in ANSI styling if colours are enabled.
fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

/// Render the statement as a human-readable table.
fn render_table(s: &CashFlowStatement, use_color: bool) -> String {
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;
    use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};

    let bold = anstyle::Style::new().bold();
    let cyan = anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)));
    let green =
        anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)));
    let red_bold = anstyle::Style::new()
        .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)))
        .bold();

    let mut out = Vec::new();
    out.push(styled(
        &format!(
            "Cash Flow Statement ({} to {}) ({})",
            s.dates().from(),
            s.dates().to(),
            s.currency().code()
        ),
        bold,
        use_color,
    ));
    let cash_list: Vec<String> = s
        .cash_accounts
        .iter()
        .map(|a| format!("{} {}", a.code, a.name))
        .collect();
    out.push(format!("Cash accounts: {}", cash_list.join(", ")));
    out.push(String::new());

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        Cell::new(styled("Code", bold, use_color)),
        Cell::new(styled("Account", bold, use_color)),
        Cell::new(styled("Amount", bold, use_color)),
    ]);

    for section in Section::ALL {
        let sec = s.section(section);
        table.add_row(vec![
            Cell::new(""),
            Cell::new(styled(section.heading(), bold, use_color)),
            Cell::new(""),
        ]);
        if sec.lines.is_empty() {
            table.add_row(vec![Cell::new(""), Cell::new("  (none)"), Cell::new("")]);
        }
        for line in &sec.lines {
            table.add_row(vec![
                Cell::new(styled(&line.code, cyan, use_color)),
                Cell::new(format!("  {}", line.name)),
                Cell::new(format_money_grouped(line.amount)).set_alignment(CellAlignment::Right),
            ]);
        }
        table.add_row(vec![
            Cell::new(""),
            Cell::new(styled(section.total_label(), bold, use_color)),
            Cell::new(styled(&format_money_grouped(sec.total), bold, use_color))
                .set_alignment(CellAlignment::Right),
        ]);
    }
    out.push(table.to_string());
    out.push(String::new());

    out.push(format!(
        "Net change in cash:  {}",
        styled(&format_money_grouped(s.net_change()), bold, use_color)
    ));
    out.push(format!(
        "Opening cash:        {}",
        format_money_grouped(s.opening_cash())
    ));
    out.push(format!(
        "Closing cash:        {}",
        styled(&format_money_grouped(s.closing_cash()), bold, use_color)
    ));
    if s.reconciled() {
        out.push(styled(
            "[ok] RECONCILED (opening + net change = closing)",
            green,
            use_color,
        ));
    } else {
        out.push(styled(
            &format!(
                "[!!] MISMATCH: opening + net change != closing (difference: {})",
                format_money_grouped(s.difference())
            ),
            red_bold,
            use_color,
        ));
    }
    out.join("\n")
}

/// Render the statement as CSV.
///
/// Columns: `row_type,section,code,name,account_type,amount`, where
/// `row_type` is `line`, `subtotal` or `summary`. Amounts are minor units.
///
/// # Errors
/// Returns [`CliError`] if CSV serialization fails.
pub fn render_csv(s: &CashFlowStatement) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    let csv_err = |e: csv::Error| CliError::General(format!("CSV write failed: {e}"));

    wtr.write_record([
        "row_type",
        "section",
        "code",
        "name",
        "account_type",
        "amount",
    ])
    .map_err(csv_err)?;
    for section in Section::ALL {
        let sec = s.section(section);
        for line in &sec.lines {
            wtr.write_record([
                "line",
                section.key(),
                &line.code,
                &line.name,
                line.account_type.code(),
                &line.amount.amount().minor_units().to_string(),
            ])
            .map_err(csv_err)?;
        }
        wtr.write_record([
            "subtotal",
            section.key(),
            "",
            section.total_label(),
            "",
            &sec.total.amount().minor_units().to_string(),
        ])
        .map_err(csv_err)?;
    }
    for (key, value) in [
        ("net_change", s.net_change),
        ("opening_cash", s.opening_cash),
        ("closing_cash", s.closing_cash),
        ("difference", s.difference),
    ] {
        wtr.write_record([
            "summary",
            "",
            "",
            key,
            "",
            &value.amount().minor_units().to_string(),
        ])
        .map_err(csv_err)?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes).map_err(|e| CliError::General(format!("CSV is not UTF-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_rejects_overflow_across_balanced_transactions() {
        let fact = |txn_id, code: &str, kind: AccountType, signed| EntryFact {
            txn_id,
            code: code.into(),
            name: code.into(),
            account_type: kind,
            signed,
        };
        let facts = [
            fact(1, "1000", AccountType::Asset, i64::MAX),
            fact(1, "4000", AccountType::Revenue, -i64::MAX),
            fact(2, "1000", AccountType::Asset, 1),
            fact(2, "4000", AccountType::Revenue, -1),
        ];
        assert!(attribute_cash(&facts, &BTreeSet::from(["1000"])).is_err());
    }

    #[test]
    fn default_cash_name_rule() {
        assert!(is_default_cash_name("Operating Cash"));
        assert!(is_default_cash_name("Checking Account"));
        assert!(is_default_cash_name("Savings Account"));
        assert!(is_default_cash_name("First National BANK"));
        assert!(!is_default_cash_name("Accounts Receivable"));
        assert!(!is_default_cash_name("Inventory"));
    }

    #[test]
    fn section_classification() {
        assert_eq!(
            Section::for_account_type(AccountType::Revenue),
            Some(Section::Operating)
        );
        assert_eq!(
            Section::for_account_type(AccountType::Expense),
            Some(Section::Operating)
        );
        assert_eq!(
            Section::for_account_type(AccountType::Asset),
            Some(Section::Investing)
        );
        assert_eq!(
            Section::for_account_type(AccountType::Liability),
            Some(Section::Financing)
        );
        assert_eq!(
            Section::for_account_type(AccountType::Equity),
            Some(Section::Financing)
        );
        assert!("other".parse::<AccountType>().is_err());
    }
}
