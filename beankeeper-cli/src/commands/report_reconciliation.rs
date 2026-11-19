//! `bk report reconciliation`: compare book, cleared, and reconciled balances
//! for a single account using entry clearance status.
//!
//! All balances are expressed on the account's normal side: debit-normal for
//! asset and expense accounts, credit-normal for liability, equity, and
//! revenue accounts. A positive number therefore means "the account holds its
//! normal balance" (money in the bank, an amount owed on a credit card, ...).
//!
//! Invariant: `book_balance - cleared_balance == uncleared_total`, where the
//! uncleared total is the sum of the signed amounts of the listed uncleared
//! entries.

use beankeeper::types::Currency;
use chrono::NaiveDate;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::cli::{Cli, OutputFormat};
use crate::commands::txn::parse_amount_to_minor;
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;

/// Arguments for the `report reconciliation` subcommand.
pub struct ReconciliationArgs<'a> {
    /// Account code to reconcile.
    pub account: &'a str,
    /// Inclusive as-of date (`YYYY-MM-DD`); `None` means all dates.
    pub to: Option<&'a str>,
    /// Statement balance in major units (e.g. `1234.56`).
    pub statement_balance: Option<&'a str>,
    /// Currency filter; `None` resolves from the company's transactions.
    pub currency: Option<&'a str>,
}

/// An uncleared entry in the reconciliation report.
#[derive(Debug, Clone, Serialize)]
pub struct UnclearedEntry {
    pub entry_id: i64,
    pub transaction_id: i64,
    pub date: String,
    pub description: String,
    pub memo: Option<String>,
    pub direction: String,
    /// Raw (always positive) entry amount in minor units.
    pub amount: i64,
    /// Amount signed relative to the account's normal side, in minor units.
    pub signed_amount: i64,
}

/// The computed reconciliation report.
#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    pub code: String,
    pub name: String,
    pub account_type: String,
    pub normal_balance: &'static str,
    pub currency: Currency,
    pub as_of: Option<String>,
    pub book_balance: i64,
    pub cleared_balance: i64,
    pub reconciled_balance: i64,
    pub uncleared_total: i64,
    pub uncleared: Vec<UnclearedEntry>,
    pub statement_balance: Option<i64>,
}

impl ReconciliationReport {
    /// `statement - cleared`, when a statement balance was supplied.
    #[must_use]
    pub fn difference(&self) -> Option<i64> {
        self.statement_balance
            .map(|s| s.saturating_sub(self.cleared_balance))
    }

    /// Whether the cleared balance matches the statement balance.
    #[must_use]
    pub fn is_reconciled(&self) -> Option<bool> {
        self.difference().map(|d| d == 0)
    }
}

/// Execute `bk report reconciliation`.
///
/// # Errors
///
/// Returns [`CliError`] for invalid arguments, a missing account, ambiguous
/// currency, or database failures.
pub fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    args: &ReconciliationArgs<'_>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let report = build_report(db.conn(), company, args)?;

    match format {
        OutputFormat::Table => println!("{}", render_table(&report, use_color)),
        OutputFormat::Json => {
            let meta = output::json::meta("report.reconciliation", Some(company));
            println!("{}", render_json(&report, meta)?);
        }
        OutputFormat::Csv => print!("{}", render_csv(&report)?),
    }

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] reconciliation report for account {} ({} uncleared entries)",
            report.code,
            report.uncleared.len()
        );
    }

    Ok(())
}

/// Compute the reconciliation report.
///
/// # Errors
///
/// Returns [`CliError`] for invalid arguments or database failures.
pub fn build_report(
    conn: &Connection,
    company: &str,
    args: &ReconciliationArgs<'_>,
) -> Result<ReconciliationReport, CliError> {
    if let Some(to) = args.to {
        NaiveDate::parse_from_str(to, "%Y-%m-%d").map_err(|_| {
            CliError::Validation(format!("invalid --to date '{to}': expected YYYY-MM-DD"))
        })?;
    }

    let account = db::get_account(conn, company, args.account)?;
    let currency = resolve_currency(conn, company, args.currency)?;
    let statement_balance = args
        .statement_balance
        .map(|s| parse_signed_amount(s, currency))
        .transpose()?;

    let debit_normal = matches!(account.account_type.as_str(), "asset" | "expense");
    let sign: i64 = if debit_normal { 1 } else { -1 };

    // Net (debit - credit) per clearance status.
    let mut stmt = conn.prepare(
        "SELECT e.status, \
                COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE -e.amount END), 0) \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id \
         WHERE e.company_slug = ?1 AND e.account_code = ?2 AND t.currency = ?3 \
           AND (?4 IS NULL OR t.date <= ?4) \
         GROUP BY e.status",
    )?;
    let rows = stmt.query_map(
        params![company, account.code, currency.code(), args.to],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )?;

    let (mut book, mut cleared, mut reconciled) = (0i64, 0i64, 0i64);
    for row in rows {
        let (status, net) = row?;
        let normal = net.checked_mul(sign).ok_or_else(overflow)?;
        book = book.checked_add(normal).ok_or_else(overflow)?;
        match status.as_str() {
            "cleared" => cleared = cleared.checked_add(normal).ok_or_else(overflow)?,
            "reconciled" => {
                cleared = cleared.checked_add(normal).ok_or_else(overflow)?;
                reconciled = reconciled.checked_add(normal).ok_or_else(overflow)?;
            }
            _ => {}
        }
    }

    let uncleared = list_uncleared(conn, company, &account.code, currency, args.to, sign)?;
    let uncleared_total = uncleared
        .iter()
        .try_fold(0i64, |acc, e| acc.checked_add(e.signed_amount))
        .ok_or_else(overflow)?;

    Ok(ReconciliationReport {
        code: account.code,
        name: account.name,
        normal_balance: if debit_normal { "debit" } else { "credit" },
        account_type: account.account_type,
        currency,
        as_of: args.to.map(str::to_string),
        book_balance: book,
        cleared_balance: cleared,
        reconciled_balance: reconciled,
        uncleared_total,
        uncleared,
        statement_balance,
    })
}

fn overflow() -> CliError {
    CliError::Validation("reconciliation totals overflow".to_string())
}

/// List uncleared entries for the account, oldest first.
fn list_uncleared(
    conn: &Connection,
    company: &str,
    code: &str,
    currency: Currency,
    to: Option<&str>,
    sign: i64,
) -> Result<Vec<UnclearedEntry>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT e.id, t.id, t.date, t.description, e.memo, e.direction, e.amount \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id \
         WHERE e.company_slug = ?1 AND e.account_code = ?2 AND t.currency = ?3 \
           AND e.status = 'uncleared' \
           AND (?4 IS NULL OR t.date <= ?4) \
         ORDER BY t.date, t.id, e.id",
    )?;
    let rows = stmt.query_map(params![company, code, currency.code(), to], |row| {
        let direction: String = row.get(5)?;
        let amount: i64 = row.get(6)?;
        let debit_net = if direction == "debit" {
            amount
        } else {
            -amount
        };
        Ok(UnclearedEntry {
            entry_id: row.get(0)?,
            transaction_id: row.get(1)?,
            date: row.get(2)?,
            description: row.get(3)?,
            memo: row.get(4)?,
            direction,
            amount,
            signed_amount: debit_net * sign,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(CliError::from)
}

/// Resolve the report currency.
///
/// An explicit `--currency` wins. Otherwise use the single distinct currency
/// of the company's transactions; several currencies are ambiguous
/// ([`CliError::Validation`]); no transactions at all defaults to USD.
fn resolve_currency(
    conn: &Connection,
    company: &str,
    explicit: Option<&str>,
) -> Result<Currency, CliError> {
    if let Some(code) = explicit {
        let upper = code.trim().to_ascii_uppercase();
        return Currency::from_code(&upper)
            .map_err(|_| CliError::Validation(format!("unknown currency: {code}")));
    }

    let mut stmt = conn.prepare(
        "SELECT DISTINCT currency FROM transactions WHERE company_slug = ?1 ORDER BY currency",
    )?;
    let codes = stmt
        .query_map(params![company], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    match codes.as_slice() {
        [] => Ok(Currency::USD),
        [code] => Currency::from_code(code)
            .map_err(|_| CliError::Validation(format!("unknown currency in ledger: {code}"))),
        many => Err(CliError::Validation(format!(
            "company '{company}' has transactions in several currencies ({}); \
             pass --currency to choose one",
            many.join(", ")
        ))),
    }
}

/// Parse a major-unit amount that may carry a leading minus sign.
///
/// Delegates to [`parse_amount_to_minor`] (the helper behind `txn list
/// --amount-gt`) for the magnitude, so sign handling stays correct for
/// fractional amounts like `-12.34`.
fn parse_signed_amount(s: &str, currency: Currency) -> Result<i64, CliError> {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix('-') {
        let magnitude = parse_amount_to_minor(rest, currency)?;
        Ok(-magnitude)
    } else {
        parse_amount_to_minor(trimmed.strip_prefix('+').unwrap_or(trimmed), currency)
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Format minor units with thousands separators and the currency's decimals.
fn format_amount(minor_units: i64, decimal_places: u8) -> String {
    let abs_units = minor_units.unsigned_abs();
    let divisor = 10u64.pow(u32::from(decimal_places));
    let whole = (abs_units / divisor).to_string();
    let frac = abs_units % divisor;

    let mut whole_str = String::with_capacity(whole.len() + whole.len() / 3);
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
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

fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

fn color(c: anstyle::AnsiColor) -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(c)))
}

/// Render the report as human-readable tables.
#[must_use]
pub fn render_table(r: &ReconciliationReport, use_color: bool) -> String {
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;
    use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};

    let bold = anstyle::Style::new().bold();
    let cyan = color(anstyle::AnsiColor::Cyan);
    let green = color(anstyle::AnsiColor::Green);
    let red_bold = color(anstyle::AnsiColor::Red).bold();
    let mu = r.currency.minor_units();
    let ccy = r.currency.code();
    let fmt = |v: i64| format!("{} {ccy}", format_amount(v, mu));

    let new_table = || {
        let mut t = Table::new();
        t.load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_content_arrangement(ContentArrangement::Dynamic);
        t
    };

    let as_of = r
        .as_of
        .as_deref()
        .map_or_else(|| "all dates".to_string(), |d| format!("through {d}"));
    let mut lines = vec![
        styled(
            &format!("Reconciliation -- {} {} ({as_of}) ({ccy})", r.code, r.name),
            bold,
            use_color,
        ),
        format!(
            "Type: {}  Normal balance: {}",
            r.account_type, r.normal_balance
        ),
        String::new(),
    ];

    let mut summary = new_table();
    summary.set_header(vec![
        Cell::new(styled("Balance", bold, use_color)),
        Cell::new(styled("Amount", bold, use_color)),
    ]);
    let mut add = |label: &str, value: String| {
        summary.add_row(vec![
            Cell::new(label),
            Cell::new(value).set_alignment(CellAlignment::Right),
        ]);
    };
    add("Book balance", fmt(r.book_balance));
    add("Cleared balance", fmt(r.cleared_balance));
    add("Reconciled balance", fmt(r.reconciled_balance));
    add("Uncleared total", fmt(r.uncleared_total));
    if let (Some(stmt), Some(diff)) = (r.statement_balance, r.difference()) {
        add("Statement balance", fmt(stmt));
        add("Difference (statement - cleared)", fmt(diff));
    }
    lines.push(summary.to_string());

    if let Some(ok) = r.is_reconciled() {
        lines.push(if ok {
            styled("[ok] reconciled: yes", green, use_color)
        } else {
            styled("[!!] reconciled: no", red_bold, use_color)
        });
    }

    lines.push(String::new());
    if r.uncleared.is_empty() {
        lines.push("No uncleared entries.".to_string());
    } else {
        lines.push(styled(
            &format!("Uncleared entries ({})", r.uncleared.len()),
            bold,
            use_color,
        ));
        let mut t = new_table();
        t.set_header(
            ["Entry", "Txn", "Date", "Description", "Amount"]
                .iter()
                .map(|h| Cell::new(styled(h, bold, use_color)))
                .collect::<Vec<_>>(),
        );
        for e in &r.uncleared {
            t.add_row(vec![
                Cell::new(e.entry_id).set_alignment(CellAlignment::Right),
                Cell::new(styled(&e.transaction_id.to_string(), cyan, use_color))
                    .set_alignment(CellAlignment::Right),
                Cell::new(&e.date),
                Cell::new(&e.description),
                Cell::new(format_amount(e.signed_amount, mu)).set_alignment(CellAlignment::Right),
            ]);
        }
        lines.push(t.to_string());
    }

    lines.join("\n")
}

#[derive(Serialize)]
struct AccountJson<'a> {
    code: &'a str,
    name: &'a str,
    #[serde(rename = "type")]
    account_type: &'a str,
    normal_balance: &'a str,
}

#[derive(Serialize)]
struct ReportJson<'a> {
    account: AccountJson<'a>,
    currency: &'a str,
    as_of: Option<&'a str>,
    book_balance: i64,
    cleared_balance: i64,
    reconciled_balance: i64,
    uncleared_total: i64,
    uncleared_count: usize,
    statement_balance: Option<i64>,
    difference: Option<i64>,
    reconciled: Option<bool>,
    uncleared_entries: &'a [UnclearedEntry],
}

/// Render the report as an enveloped JSON document.
///
/// # Errors
///
/// Returns [`CliError::General`] on serialisation failure.
pub fn render_json(r: &ReconciliationReport, meta: output::json::Meta) -> Result<String, CliError> {
    let data = ReportJson {
        account: AccountJson {
            code: &r.code,
            name: &r.name,
            account_type: &r.account_type,
            normal_balance: r.normal_balance,
        },
        currency: r.currency.code(),
        as_of: r.as_of.as_deref(),
        book_balance: r.book_balance,
        cleared_balance: r.cleared_balance,
        reconciled_balance: r.reconciled_balance,
        uncleared_total: r.uncleared_total,
        uncleared_count: r.uncleared.len(),
        statement_balance: r.statement_balance,
        difference: r.difference(),
        reconciled: r.is_reconciled(),
        uncleared_entries: &r.uncleared,
    };
    let env = output::json::Envelope {
        ok: true,
        meta,
        data: Some(data),
        error: None,
    };
    serde_json::to_string_pretty(&env)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

/// Render the report as CSV.
///
/// A single header row, then one `entry` row per uncleared entry, followed by
/// `summary` rows whose `description` column names the metric
/// (`book_balance`, `cleared_balance`, `reconciled_balance`,
/// `uncleared_total`, and, with a statement balance, `statement_balance`,
/// `difference`, `reconciled` with amount `1`/`0`). Amounts are signed minor
/// units on the account's normal side.
///
/// Columns: `row_type`, `entry_id`, `transaction_id`, `date`, `description`,
/// `amount`, `currency`
///
/// # Errors
///
/// Returns [`CliError::General`] if CSV serialisation fails.
pub fn render_csv(r: &ReconciliationReport) -> Result<String, CliError> {
    let csv_err = |e: csv::Error| CliError::General(format!("CSV serialization failed: {e}"));
    let ccy = r.currency.code();
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record([
        "row_type",
        "entry_id",
        "transaction_id",
        "date",
        "description",
        "amount",
        "currency",
    ])
    .map_err(csv_err)?;

    for e in &r.uncleared {
        wtr.write_record([
            "entry",
            &e.entry_id.to_string(),
            &e.transaction_id.to_string(),
            &e.date,
            &e.description,
            &e.signed_amount.to_string(),
            ccy,
        ])
        .map_err(csv_err)?;
    }

    let as_of = r.as_of.as_deref().unwrap_or("");
    let mut summary = vec![
        ("book_balance", r.book_balance),
        ("cleared_balance", r.cleared_balance),
        ("reconciled_balance", r.reconciled_balance),
        ("uncleared_total", r.uncleared_total),
    ];
    if let (Some(stmt), Some(diff), Some(ok)) =
        (r.statement_balance, r.difference(), r.is_reconciled())
    {
        summary.push(("statement_balance", stmt));
        summary.push(("difference", diff));
        summary.push(("reconciled", i64::from(ok)));
    }
    for (label, value) in summary {
        wtr.write_record(["summary", "", "", as_of, label, &value.to_string(), ccy])
            .map_err(csv_err)?;
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

    #[test]
    fn signed_amount_parsing() {
        assert_eq!(
            parse_signed_amount("1234.56", Currency::USD).ok(),
            Some(123_456)
        );
        assert_eq!(
            parse_signed_amount("-12.34", Currency::USD).ok(),
            Some(-1234)
        );
        assert_eq!(parse_signed_amount("+5", Currency::USD).ok(), Some(500));
        assert!(parse_signed_amount("1.234", Currency::USD).is_err());
        assert!(parse_signed_amount("abc", Currency::USD).is_err());
    }

    #[test]
    fn amount_formatting() {
        assert_eq!(format_amount(123_456, 2), "1,234.56");
        assert_eq!(format_amount(-5, 2), "-0.05");
        assert_eq!(format_amount(1000, 0), "1,000");
    }
}
