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

use beankeeper::types::Currency;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::cli::{Cli, OutputFormat};
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;

/// Substrings (lowercase) that mark an asset account as cash by default.
pub const DEFAULT_CASH_NAME_KEYWORDS: &[&str] = &["cash", "bank", "checking", "savings"];

/// Arguments for `bk report cash-flow`.
#[derive(clap::Args, Debug)]
pub struct CashFlowArgs {
    /// Start date (inclusive, YYYY-MM-DD).
    #[arg(long)]
    pub from: String,

    /// End date (inclusive, YYYY-MM-DD).
    #[arg(long)]
    pub to: String,

    /// Account code to treat as cash (repeatable). Defaults to asset accounts
    /// whose name contains "cash", "bank", "checking" or "savings".
    #[arg(long = "cash-account", num_args = 1)]
    pub cash_account: Vec<String>,

    /// Currency code. Defaults to the company's only transaction currency
    /// (USD when there are no transactions).
    #[arg(long)]
    pub currency: Option<String>,
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

    fn for_account_type(account_type: &str) -> Option<Self> {
        match account_type {
            "revenue" | "expense" => Some(Self::Operating),
            "asset" => Some(Self::Investing),
            "liability" | "equity" => Some(Self::Financing),
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
#[derive(Debug, Clone, Serialize)]
pub struct CashFlowLine {
    /// Counter-account code.
    pub code: String,
    /// Counter-account name.
    pub name: String,
    /// Counter-account type (lowercase).
    pub account_type: String,
    /// Net cash movement in minor units (positive = inflow).
    pub amount: i64,
}

/// One section of the statement.
#[derive(Debug, Clone, Serialize)]
pub struct CashFlowSection {
    /// Line items, ordered by account code.
    pub lines: Vec<CashFlowLine>,
    /// Section subtotal in minor units.
    pub total: i64,
}

/// The computed cash flow statement.
#[derive(Debug, Clone, Serialize)]
pub struct CashFlowStatement {
    /// Start date (inclusive).
    pub from: String,
    /// End date (inclusive).
    pub to: String,
    /// Report currency code.
    pub currency: String,
    /// Cash accounts used.
    pub cash_accounts: Vec<CashAccount>,
    /// Operating activities.
    pub operating: CashFlowSection,
    /// Investing activities.
    pub investing: CashFlowSection,
    /// Financing activities.
    pub financing: CashFlowSection,
    /// Sum of the three section totals.
    pub net_change: i64,
    /// Cash balance before `from`.
    pub opening_cash: i64,
    /// Cash balance through `to`, computed independently.
    pub closing_cash: i64,
    /// Whether `opening_cash + net_change == closing_cash`.
    pub reconciled: bool,
    /// `closing_cash - (opening_cash + net_change)`; zero when reconciled.
    pub difference: i64,
}

impl CashFlowStatement {
    fn section(&self, section: Section) -> &CashFlowSection {
        match section {
            Section::Operating => &self.operating,
            Section::Investing => &self.investing,
            Section::Financing => &self.financing,
        }
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
    let conn = db.conn();
    if !db::company_exists(conn, company)? {
        return Err(CliError::NotFound(format!("company '{company}' not found")));
    }

    let from = parse_date("--from", &args.from)?;
    let to = parse_date("--to", &args.to)?;
    if from > to {
        return Err(CliError::Validation(format!(
            "--from ({from}) must be on or before --to ({to})"
        )));
    }

    let currency = resolve_currency(conn, company, args.currency.as_deref())?;
    let cash_accounts = resolve_cash_accounts(conn, company, &args.cash_account)?;
    let statement = compute_cash_flow(
        conn,
        company,
        &from.to_string(),
        &to.to_string(),
        currency,
        cash_accounts,
    )?;
    let minor_units = currency.minor_units();

    match format {
        OutputFormat::Table => println!("{}", render_table(&statement, minor_units, use_color)),
        OutputFormat::Json => {
            let meta = output::json::meta("report.cash-flow", Some(company));
            let env = output::json::Envelope {
                ok: true,
                meta,
                data: Some(&statement),
                error: None,
            };
            let rendered = serde_json::to_string_pretty(&env)
                .map_err(|e| CliError::General(format!("JSON serialisation failed: {e}")))?;
            println!("{rendered}");
        }
        OutputFormat::Csv => print!("{}", render_csv(&statement)?),
    }

    if !cli.verbosity.quiet {
        if statement.reconciled {
            eprintln!("[ok] cash flow statement generated");
        } else {
            eprintln!(
                "[!!] cash flow statement does not reconcile (difference: {})",
                statement.difference
            );
        }
    }

    Ok(())
}

/// Parse a `YYYY-MM-DD` date argument.
fn parse_date(flag: &str, value: &str) -> Result<chrono::NaiveDate, CliError> {
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
        CliError::Validation(format!(
            "{flag} must be a date in YYYY-MM-DD format, got '{value}'"
        ))
    })
}

/// Resolve the report currency via the shared report resolver
/// ([`super::report_currency::resolve_report_currency`]).
fn resolve_currency(
    conn: &Connection,
    company: &str,
    explicit: Option<&str>,
) -> Result<Currency, CliError> {
    super::report_currency::resolve_report_currency(conn, company, explicit)
}

/// Whether an account name matches the default cash-account rule.
fn is_default_cash_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    DEFAULT_CASH_NAME_KEYWORDS
        .iter()
        .any(|kw| lower.contains(kw))
}

/// Resolve the set of cash accounts (explicit or by the default rule).
fn resolve_cash_accounts(
    conn: &Connection,
    company: &str,
    explicit: &[String],
) -> Result<Vec<CashAccount>, CliError> {
    let mut stmt = conn
        .prepare("SELECT code, name, type FROM accounts WHERE company_slug = ?1 ORDER BY code")?;
    let accounts = stmt
        .query_map(params![company], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    if explicit.is_empty() {
        let found: Vec<CashAccount> = accounts
            .into_iter()
            .filter(|(_, name, ty)| ty == "asset" && is_default_cash_name(name))
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

    let wanted: BTreeSet<&str> = explicit.iter().map(String::as_str).collect();
    let mut found = Vec::with_capacity(wanted.len());
    for code in wanted {
        let Some((code, name, ty)) = accounts.iter().find(|(c, _, _)| c == code) else {
            return Err(CliError::NotFound(format!(
                "cash account '{code}' not found in company '{company}'"
            )));
        };
        if ty != "asset" {
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
    account_type: String,
    /// Debit = positive, credit = negative.
    signed: i64,
}

/// Compute the statement from the database.
fn compute_cash_flow(
    conn: &Connection,
    company: &str,
    from: &str,
    to: &str,
    currency: Currency,
    cash_accounts: Vec<CashAccount>,
) -> Result<CashFlowStatement, CliError> {
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
        .query_map(params![company, currency.code(), from, to], |row| {
            Ok(EntryFact {
                txn_id: row.get(0)?,
                code: row.get(1)?,
                name: row.get(2)?,
                account_type: row.get(3)?,
                signed: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let lines = attribute_cash(&facts, &cash_codes)?;

    let mut sections: BTreeMap<Section, CashFlowSection> = Section::ALL
        .iter()
        .map(|s| {
            (
                *s,
                CashFlowSection {
                    lines: Vec::new(),
                    total: 0,
                },
            )
        })
        .collect();
    for ((section, _), line) in lines {
        if let Some(sec) = sections.get_mut(&section) {
            sec.total = sec.total.saturating_add(line.amount);
            sec.lines.push(line);
        }
    }
    let mut take = |s: Section| {
        sections.remove(&s).unwrap_or(CashFlowSection {
            lines: Vec::new(),
            total: 0,
        })
    };
    let operating = take(Section::Operating);
    let investing = take(Section::Investing);
    let financing = take(Section::Financing);

    let net_change = operating
        .total
        .saturating_add(investing.total)
        .saturating_add(financing.total);
    let opening_cash = cash_balance(conn, company, currency, &cash_codes, false, from)?;
    let closing_cash = cash_balance(conn, company, currency, &cash_codes, true, to)?;
    let difference = closing_cash.saturating_sub(opening_cash.saturating_add(net_change));

    Ok(CashFlowStatement {
        from: from.to_string(),
        to: to.to_string(),
        currency: currency.code().to_string(),
        cash_accounts,
        operating,
        investing,
        financing,
        net_change,
        opening_cash,
        closing_cash,
        reconciled: difference == 0,
        difference,
    })
}

/// Attribute each in-range transaction's net cash movement to its non-cash
/// counter-accounts, keyed by `(section, account code)`.
fn attribute_cash(
    facts: &[EntryFact],
    cash_codes: &BTreeSet<&str>,
) -> Result<BTreeMap<(Section, String), CashFlowLine>, CliError> {
    // Group entries by transaction.
    let mut by_txn: BTreeMap<i64, Vec<&EntryFact>> = BTreeMap::new();
    for fact in facts {
        by_txn.entry(fact.txn_id).or_default().push(fact);
    }

    // (section, code) -> line
    let mut lines: BTreeMap<(Section, String), CashFlowLine> = BTreeMap::new();
    for entries in by_txn.values() {
        let cash_net: i64 = entries
            .iter()
            .filter(|e| cash_codes.contains(e.code.as_str()))
            .map(|e| e.signed)
            .sum();
        if cash_net == 0 {
            // No cash touched, or a cash-to-cash transfer: no cash flow.
            continue;
        }
        for entry in entries
            .iter()
            .filter(|e| !cash_codes.contains(e.code.as_str()))
        {
            let section = Section::for_account_type(&entry.account_type).ok_or_else(|| {
                CliError::Validation(format!(
                    "account '{}' has unknown type '{}'",
                    entry.code, entry.account_type
                ))
            })?;
            let line = lines
                .entry((section, entry.code.clone()))
                .or_insert_with(|| CashFlowLine {
                    code: entry.code.clone(),
                    name: entry.name.clone(),
                    account_type: entry.account_type.clone(),
                    amount: 0,
                });
            // A credit to the counter-account is a cash inflow.
            line.amount = line.amount.saturating_sub(entry.signed);
        }
    }

    Ok(lines)
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
        total = total.saturating_add(bal);
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Format minor units with thousands separators and decimal places.
fn format_amount(minor_units: i64, decimal_places: u8) -> String {
    let abs_units = minor_units.unsigned_abs();
    let divisor = 10u64.pow(u32::from(decimal_places));
    let whole = (abs_units / divisor).to_string();
    let frac = abs_units % divisor;

    let len = whole.len();
    let mut whole_str = String::with_capacity(len + len / 3);
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            whole_str.push(',');
        }
        whole_str.push(ch);
    }

    let formatted = if decimal_places == 0 {
        whole_str
    } else {
        format!(
            "{whole_str}.{frac:0>width$}",
            width = usize::from(decimal_places)
        )
    };
    if minor_units < 0 {
        format!("-{formatted}")
    } else {
        formatted
    }
}

/// Wrap `text` in ANSI styling if colours are enabled.
fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

/// Render the statement as a human-readable table.
fn render_table(s: &CashFlowStatement, minor_units: u8, use_color: bool) -> String {
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
    let amount = |v: i64| format_amount(v, minor_units);

    let mut out = Vec::new();
    out.push(styled(
        &format!(
            "Cash Flow Statement ({} to {}) ({})",
            s.from, s.to, s.currency
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
                Cell::new(amount(line.amount)).set_alignment(CellAlignment::Right),
            ]);
        }
        table.add_row(vec![
            Cell::new(""),
            Cell::new(styled(section.total_label(), bold, use_color)),
            Cell::new(styled(&amount(sec.total), bold, use_color))
                .set_alignment(CellAlignment::Right),
        ]);
    }
    out.push(table.to_string());
    out.push(String::new());

    out.push(format!(
        "Net change in cash:  {}",
        styled(&amount(s.net_change), bold, use_color)
    ));
    out.push(format!("Opening cash:        {}", amount(s.opening_cash)));
    out.push(format!(
        "Closing cash:        {}",
        styled(&amount(s.closing_cash), bold, use_color)
    ));
    if s.reconciled {
        out.push(styled(
            "[ok] RECONCILED (opening + net change = closing)",
            green,
            use_color,
        ));
    } else {
        out.push(styled(
            &format!(
                "[!!] MISMATCH: opening + net change != closing (difference: {})",
                amount(s.difference)
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
fn render_csv(s: &CashFlowStatement) -> Result<String, CliError> {
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
                &line.account_type,
                &line.amount.to_string(),
            ])
            .map_err(csv_err)?;
        }
        wtr.write_record([
            "subtotal",
            section.key(),
            "",
            section.total_label(),
            "",
            &sec.total.to_string(),
        ])
        .map_err(csv_err)?;
    }
    for (key, value) in [
        ("net_change", s.net_change),
        ("opening_cash", s.opening_cash),
        ("closing_cash", s.closing_cash),
        ("difference", s.difference),
    ] {
        wtr.write_record(["summary", "", "", key, "", &value.to_string()])
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
            Section::for_account_type("revenue"),
            Some(Section::Operating)
        );
        assert_eq!(
            Section::for_account_type("expense"),
            Some(Section::Operating)
        );
        assert_eq!(Section::for_account_type("asset"), Some(Section::Investing));
        assert_eq!(
            Section::for_account_type("liability"),
            Some(Section::Financing)
        );
        assert_eq!(
            Section::for_account_type("equity"),
            Some(Section::Financing)
        );
        assert_eq!(Section::for_account_type("other"), None);
    }

    #[test]
    fn amount_formatting() {
        assert_eq!(format_amount(123_456, 2), "1,234.56");
        assert_eq!(format_amount(-5, 2), "-0.05");
        assert_eq!(format_amount(5000, 0), "5,000");
        assert_eq!(format_amount(0, 2), "0.00");
    }
}
