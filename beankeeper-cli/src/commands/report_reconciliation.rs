//! `bk report reconciliation`: compare book, cleared, and reconciled balances
//! for a single account using entry clearings (issue #25).
//!
//! All balances are expressed on the account's normal side: debit-normal for
//! asset and expense accounts, credit-normal for liability, equity, and
//! revenue accounts. A positive number therefore means "the account holds its
//! normal balance" (money in the bank, an amount owed on a credit card, ...).
//!
//! Clearing is per part: an entry may be cleared by several clearings
//! (`entry_clearings`), on different dates, by different statements. With
//! `--to`, only entries dated on or before it count; a clearing counts toward
//! the cleared balance when `cleared_date <= --to`, and toward the reconciled
//! balance only when it is reconciled **and** `reconciled_date <= --to`. The
//! report therefore answers "what was cleared, and what was reconciled, as of
//! that date": a clearing reconciled after `--to` shows as cleared but not
//! reconciled. With `--statement`, the cleared and reconciled balances only
//! count that statement's clearings (same date rules); outstanding items
//! still reflect every clearing.
//!
//! Invariant: `book_balance - total_cleared_balance == uncleared_total`, where
//! the uncleared total is the sum of the listed items' `signed_remaining`.
//! Without `--statement`, `cleared_balance == total_cleared_balance`.

use std::collections::BTreeMap;

use beankeeper::types::Currency;
use chrono::NaiveDate;
use rusqlite::Connection;
use serde::Serialize;

use crate::cli::{Cli, OutputFormat};
use crate::commands::txn::parse_amount_to_minor;
use crate::db;
use crate::db::connection::Db;
use crate::db::metadata_filter::{self, MetadataFilter};
use crate::error::CliError;
use crate::output;

/// Group label for outstanding items whose metadata lacks the grouping key.
pub const NO_GROUP: &str = "(unassigned)";

/// Arguments for the `report reconciliation` subcommand.
#[derive(Default)]
pub struct ReconciliationArgs<'a> {
    /// Account code to reconcile.
    pub account: &'a str,
    /// Inclusive as-of date (`YYYY-MM-DD`); `None` means all dates.
    pub to: Option<&'a str>,
    /// Statement balance in major units (e.g. `1234.56`).
    pub statement_balance: Option<&'a str>,
    /// Only count this statement's clearings in the cleared balance.
    pub statement: Option<&'a str>,
    /// `KEY=VALUE` metadata filters (AND-ed).
    pub filter_metadata: &'a [String],
    /// Group outstanding items by this metadata key.
    pub group_by_metadata: Option<&'a str>,
    /// Currency filter; `None` resolves from the company's transactions.
    pub currency: Option<&'a str>,
}

/// An outstanding (not fully cleared) entry in the reconciliation report.
#[derive(Debug, Clone, Serialize)]
pub struct UnclearedEntry {
    pub entry_id: i64,
    pub transaction_id: i64,
    pub date: String,
    pub reference: Option<String>,
    pub description: String,
    pub memo: Option<String>,
    /// Transaction metadata: parsed JSON when valid, else the raw string.
    pub metadata: Option<serde_json::Value>,
    pub direction: String,
    /// Raw (always positive) entry amount in minor units.
    pub amount: i64,
    /// Entry amount signed relative to the account's normal side.
    pub signed_amount: i64,
    /// Raw amount cleared as of the report date (all statements).
    pub cleared_amount: i64,
    /// Raw amount reconciled as of the report date (all statements).
    pub reconciled_amount: i64,
    /// Raw amount still uncleared: `amount - cleared_amount`.
    pub remaining_amount: i64,
    /// `remaining_amount` signed relative to the account's normal side.
    pub signed_remaining: i64,
    /// Whether part (but not all) of the entry is cleared.
    pub partially_cleared: bool,
    /// The entry's clearings as of the report date (oldest first).
    pub clearings: Vec<ItemClearing>,
    /// Group value when `--group-by-metadata` is set.
    #[serde(skip)]
    pub group: Option<String>,
}

/// One clearing of an outstanding entry, as of the report date: a clearing
/// reconciled after `--to` shows as `cleared` with no `reconciled_date`.
#[derive(Debug, Clone, Serialize)]
pub struct ItemClearing {
    pub id: i64,
    /// Raw (always positive) amount in minor units.
    pub amount: i64,
    pub cleared_date: String,
    pub reconciled_date: Option<String>,
    #[serde(rename = "statement")]
    pub statement_ref: Option<String>,
    pub status: &'static str,
}

/// The clearings of an entry as of `to` (`None`: current state).
fn clearings_as_of(
    conn: &Connection,
    entry_id: i64,
    to: Option<&str>,
) -> Result<Vec<ItemClearing>, CliError> {
    let on_or_before = |d: &str| to.is_none_or(|to| d <= to);
    Ok(db::clearings::list_clearings(conn, entry_id)?
        .into_iter()
        .filter(|c| on_or_before(&c.cleared_date))
        .map(|c| {
            let reconciled_date = c.reconciled_date.filter(|d| on_or_before(d));
            ItemClearing {
                id: c.id,
                amount: c.amount,
                cleared_date: c.cleared_date,
                status: if reconciled_date.is_some() {
                    "reconciled"
                } else {
                    "cleared"
                },
                reconciled_date,
                statement_ref: c.statement_ref,
            }
        })
        .collect())
}

/// Subtotal for one `--group-by-metadata` value.
#[derive(Debug, Clone, Serialize)]
pub struct GroupTotal {
    pub value: String,
    pub count: usize,
    /// Sum of `signed_remaining` over the group's items.
    pub uncleared_total: i64,
    pub entry_ids: Vec<i64>,
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
    pub statement: Option<String>,
    pub filters: Vec<MetadataFilter>,
    pub group_by: Option<String>,
    pub book_balance: i64,
    /// Cleared balance (only `statement`'s clearings when set).
    pub cleared_balance: i64,
    /// Cleared balance across all statements.
    pub total_cleared_balance: i64,
    pub reconciled_balance: i64,
    pub uncleared_total: i64,
    pub uncleared: Vec<UnclearedEntry>,
    pub groups: Option<Vec<GroupTotal>>,
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

    /// Number of listed items that are partially cleared.
    #[must_use]
    pub fn partially_cleared_count(&self) -> usize {
        self.uncleared
            .iter()
            .filter(|e| e.partially_cleared)
            .count()
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

// ---------------------------------------------------------------------------
// Metadata helpers
// ---------------------------------------------------------------------------

/// Parse metadata leniently: a JSON object, or `None` for anything else.
fn metadata_object(raw: Option<&str>) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(raw?) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The string form of a top-level metadata value: strings as-is, numbers and
/// booleans as their JSON text; `None` for missing, null, blank, or nested
/// values.
#[must_use]
pub fn metadata_value(raw: Option<&str>, key: &str) -> Option<String> {
    let map = metadata_object(raw)?;
    match map.get(key)? {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Metadata for output: parsed JSON when valid, else the raw string.
fn metadata_for_output(raw: Option<&str>) -> Option<serde_json::Value> {
    raw.map(|m| {
        serde_json::from_str(m).unwrap_or_else(|_| serde_json::Value::String(m.to_string()))
    })
}

/// Group outstanding items by their group value, sorted by value with
/// [`NO_GROUP`] last.
fn group_items(items: &[UnclearedEntry]) -> Result<Vec<GroupTotal>, CliError> {
    let mut by_value: BTreeMap<(bool, String), GroupTotal> = BTreeMap::new();
    for e in items {
        let value = e.group.clone().unwrap_or_else(|| NO_GROUP.to_string());
        let g = by_value
            .entry((e.group.is_none(), value.clone()))
            .or_insert_with(|| GroupTotal {
                value,
                count: 0,
                uncleared_total: 0,
                entry_ids: Vec::new(),
            });
        g.count += 1;
        g.uncleared_total = g
            .uncleared_total
            .checked_add(e.signed_remaining)
            .ok_or_else(overflow)?;
        g.entry_ids.push(e.entry_id);
    }
    Ok(by_value.into_values().collect())
}

// ---------------------------------------------------------------------------
// Computation
// ---------------------------------------------------------------------------

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
    let filters = metadata_filter::parse_filters(args.filter_metadata)?;
    let statement = args
        .statement
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let account = db::get_account(conn, company, args.account)?;
    let currency = resolve_currency(conn, company, args.currency)?;
    let statement_balance = args
        .statement_balance
        .map(|s| parse_signed_amount(s, currency))
        .transpose()?;

    let debit_normal = matches!(account.account_type.as_str(), "asset" | "expense");
    let sign: i64 = if debit_normal { 1 } else { -1 };

    let rows = load_entries(
        conn,
        company,
        &EntryQuery {
            code: &account.code,
            currency,
            to: args.to,
            statement: statement.as_deref(),
            filters: &filters,
        },
    )?;

    let (mut book, mut cleared, mut total_cleared, mut reconciled) = (0i64, 0i64, 0i64, 0i64);
    let mut uncleared = Vec::new();
    for r in rows {
        let dsign = if r.direction == "debit" { sign } else { -sign };
        let (stmt_cleared, stmt_reconciled) = if statement.is_some() {
            (r.cleared_stmt, r.reconciled_stmt)
        } else {
            (r.cleared_all, r.reconciled_all)
        };
        book = add(book, r.amount, dsign)?;
        total_cleared = add(total_cleared, r.cleared_all, dsign)?;
        cleared = add(cleared, stmt_cleared, dsign)?;
        reconciled = add(reconciled, stmt_reconciled, dsign)?;

        let remaining = r.amount - r.cleared_all;
        if remaining > 0 {
            let group = args
                .group_by_metadata
                .and_then(|k| metadata_value(r.metadata.as_deref(), k));
            let clearings = clearings_as_of(conn, r.entry_id, args.to)?;
            uncleared.push(UnclearedEntry {
                entry_id: r.entry_id,
                transaction_id: r.transaction_id,
                date: r.date,
                reference: r.reference,
                description: r.description,
                memo: r.memo,
                metadata: metadata_for_output(r.metadata.as_deref()),
                direction: r.direction,
                amount: r.amount,
                signed_amount: r.amount * dsign,
                cleared_amount: r.cleared_all,
                reconciled_amount: r.reconciled_all,
                remaining_amount: remaining,
                signed_remaining: remaining * dsign,
                partially_cleared: r.cleared_all > 0,
                clearings,
                group,
            });
        }
    }

    let uncleared_total = uncleared
        .iter()
        .try_fold(0i64, |acc, e| acc.checked_add(e.signed_remaining))
        .ok_or_else(overflow)?;
    let groups = args
        .group_by_metadata
        .map(|_| group_items(&uncleared))
        .transpose()?;

    Ok(ReconciliationReport {
        code: account.code,
        name: account.name,
        normal_balance: if debit_normal { "debit" } else { "credit" },
        account_type: account.account_type,
        currency,
        as_of: args.to.map(str::to_string),
        statement,
        filters,
        group_by: args.group_by_metadata.map(str::to_string),
        book_balance: book,
        cleared_balance: cleared,
        total_cleared_balance: total_cleared,
        reconciled_balance: reconciled,
        uncleared_total,
        uncleared,
        groups,
        statement_balance,
    })
}

fn overflow() -> CliError {
    CliError::Validation("reconciliation totals overflow".to_string())
}

fn add(acc: i64, amount: i64, sign: i64) -> Result<i64, CliError> {
    amount
        .checked_mul(sign)
        .and_then(|v| acc.checked_add(v))
        .ok_or_else(overflow)
}

/// One entry with its clearing aggregates as of the report date.
struct EntryRow {
    entry_id: i64,
    transaction_id: i64,
    date: String,
    description: String,
    reference: Option<String>,
    metadata: Option<String>,
    memo: Option<String>,
    direction: String,
    amount: i64,
    cleared_all: i64,
    reconciled_all: i64,
    cleared_stmt: i64,
    reconciled_stmt: i64,
}

/// Which entries [`load_entries`] reads: one account in one currency, up to
/// `to`, matching the metadata filters; `statement` scopes the per-statement
/// clearing sums.
#[derive(Clone, Copy)]
struct EntryQuery<'a> {
    code: &'a str,
    currency: Currency,
    to: Option<&'a str>,
    statement: Option<&'a str>,
    filters: &'a [MetadataFilter],
}

/// Load the account's entries (oldest first) with clearing sums as of `to`:
/// cleared sums count clearings with `cleared_date <= to`, reconciled sums
/// only reconciled clearings with `reconciled_date <= to`.
fn load_entries(
    conn: &Connection,
    company: &str,
    query: &EntryQuery<'_>,
) -> Result<Vec<EntryRow>, CliError> {
    let EntryQuery {
        code,
        currency,
        to,
        statement,
        filters,
    } = *query;
    let filter = metadata_filter::sql_predicate("t.metadata", filters, 6);
    let mut stmt = conn.prepare(&format!(
        "SELECT e.id, t.id, t.date, t.description, t.reference, t.metadata, e.memo, \
                e.direction, e.amount, \
                COALESCE(c.cleared_all, 0), COALESCE(c.reconciled_all, 0), \
                COALESCE(c.cleared_stmt, 0), COALESCE(c.reconciled_stmt, 0) \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id \
         LEFT JOIN ( \
             SELECT entry_id, \
                    SUM(amount) AS cleared_all, \
                    SUM(CASE WHEN status = 'reconciled' \
                              AND (?4 IS NULL OR reconciled_date <= ?4) \
                             THEN amount ELSE 0 END) AS reconciled_all, \
                    SUM(CASE WHEN statement_ref = ?5 THEN amount ELSE 0 END) AS cleared_stmt, \
                    SUM(CASE WHEN statement_ref = ?5 AND status = 'reconciled' \
                              AND (?4 IS NULL OR reconciled_date <= ?4) \
                             THEN amount ELSE 0 END) AS reconciled_stmt \
             FROM entry_clearings \
             WHERE company_slug = ?1 AND (?4 IS NULL OR cleared_date <= ?4) \
             GROUP BY entry_id \
         ) c ON c.entry_id = e.id \
         WHERE e.company_slug = ?1 AND e.account_code = ?2 AND t.currency = ?3 \
           AND (?4 IS NULL OR t.date <= ?4) AND {} \
         ORDER BY t.date, t.id, e.id",
        filter.sql
    ))?;
    let mut values: Vec<Option<String>> = vec![
        Some(company.to_string()),
        Some(code.to_string()),
        Some(currency.code().to_string()),
        to.map(str::to_string),
        statement.map(str::to_string),
    ];
    values.extend(filter.params.into_iter().map(Some));
    let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
        Ok(EntryRow {
            entry_id: row.get(0)?,
            transaction_id: row.get(1)?,
            date: row.get(2)?,
            description: row.get(3)?,
            reference: row.get(4)?,
            metadata: row.get(5)?,
            memo: row.get(6)?,
            direction: row.get(7)?,
            amount: row.get(8)?,
            cleared_all: row.get(9)?,
            reconciled_all: row.get(10)?,
            cleared_stmt: row.get(11)?,
            reconciled_stmt: row.get(12)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(CliError::from)
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

/// Parse a major-unit amount that may carry a leading minus sign
/// (e.g. an overdrawn account's statement balance, `-12.34`).
fn parse_signed_amount(s: &str, currency: Currency) -> Result<i64, CliError> {
    parse_amount_to_minor(s.trim(), currency)
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

/// Compact single-line metadata for table/CSV cells.
fn metadata_cell(m: Option<&serde_json::Value>) -> String {
    match m {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

fn new_table() -> comfy_table::Table {
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;
    let mut t = comfy_table::Table::new();
    t.load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
    t
}

/// Amount with the currency code, e.g. `1,234.56 USD`.
fn with_currency(r: &ReconciliationReport, v: i64) -> String {
    format!(
        "{} {}",
        format_amount(v, r.currency.minor_units()),
        r.currency.code()
    )
}

/// Title, account type, statement and filter lines.
fn header_lines(r: &ReconciliationReport, use_color: bool) -> Vec<String> {
    let bold = anstyle::Style::new().bold();
    let as_of = r
        .as_of
        .as_deref()
        .map_or_else(|| "all dates".to_string(), |d| format!("through {d}"));
    let mut lines = vec![
        styled(
            &format!(
                "Reconciliation -- {} {} ({as_of}) ({})",
                r.code,
                r.name,
                r.currency.code()
            ),
            bold,
            use_color,
        ),
        format!(
            "Type: {}  Normal balance: {}",
            r.account_type, r.normal_balance
        ),
    ];
    if let Some(s) = &r.statement {
        lines.push(format!("Statement: {s}"));
    }
    if !r.filters.is_empty() {
        lines.push(format!(
            "Metadata filter: {}",
            metadata_filter::describe(&r.filters)
        ));
    }
    lines
}

/// The balances table.
fn balances_table(r: &ReconciliationReport, use_color: bool) -> String {
    use comfy_table::{Cell, CellAlignment};
    let bold = anstyle::Style::new().bold();
    let mut summary = new_table();
    summary.set_header(vec![
        Cell::new(styled("Balance", bold, use_color)),
        Cell::new(styled("Amount", bold, use_color)),
    ]);
    let mut add = |label: &str, value: i64| {
        summary.add_row(vec![
            Cell::new(label),
            Cell::new(with_currency(r, value)).set_alignment(CellAlignment::Right),
        ]);
    };
    add("Book balance", r.book_balance);
    if r.statement.is_some() {
        add("Cleared balance (this statement)", r.cleared_balance);
        add("Cleared balance (all statements)", r.total_cleared_balance);
    } else {
        add("Cleared balance", r.cleared_balance);
    }
    add("Reconciled balance", r.reconciled_balance);
    add("Uncleared total", r.uncleared_total);
    if let (Some(stmt), Some(diff)) = (r.statement_balance, r.difference()) {
        add("Statement balance", stmt);
        add("Difference (statement - cleared)", diff);
    }
    summary.to_string()
}

/// A table of outstanding entries.
fn items_table(items: &[&UnclearedEntry], minor_units: u8, use_color: bool) -> String {
    use comfy_table::{Cell, CellAlignment};
    let bold = anstyle::Style::new().bold();
    let cyan = color(anstyle::AnsiColor::Cyan);
    let mut t = new_table();
    t.set_header(
        [
            "Entry",
            "Txn",
            "Date",
            "Reference",
            "Description",
            "Amount",
            "Cleared",
            "Remaining",
            "Metadata",
        ]
        .iter()
        .map(|h| Cell::new(styled(h, bold, use_color)))
        .collect::<Vec<_>>(),
    );
    let amount =
        |v: i64| Cell::new(format_amount(v, minor_units)).set_alignment(CellAlignment::Right);
    for e in items {
        let cleared = if e.partially_cleared {
            format_amount(e.cleared_amount * e.signed_amount.signum(), minor_units)
        } else {
            String::new()
        };
        t.add_row(vec![
            Cell::new(e.entry_id).set_alignment(CellAlignment::Right),
            Cell::new(styled(&e.transaction_id.to_string(), cyan, use_color))
                .set_alignment(CellAlignment::Right),
            Cell::new(&e.date),
            Cell::new(e.reference.as_deref().unwrap_or("")),
            Cell::new(&e.description),
            amount(e.signed_amount),
            Cell::new(cleared).set_alignment(CellAlignment::Right),
            amount(e.signed_remaining),
            Cell::new(metadata_cell(e.metadata.as_ref())),
        ]);
    }
    t.to_string()
}

/// The outstanding entries, grouped when `--group-by-metadata` is set.
fn uncleared_lines(r: &ReconciliationReport, use_color: bool) -> Vec<String> {
    let bold = anstyle::Style::new().bold();
    let mu = r.currency.minor_units();
    if r.uncleared.is_empty() {
        return vec!["No uncleared entries.".to_string()];
    }
    let partial = r.partially_cleared_count();
    let partial_note = if partial > 0 {
        format!(", {partial} partially cleared")
    } else {
        String::new()
    };
    let mut lines = vec![styled(
        &format!("Uncleared entries ({}{partial_note})", r.uncleared.len()),
        bold,
        use_color,
    )];

    if let (Some(groups), Some(key)) = (&r.groups, &r.group_by) {
        for g in groups {
            let items: Vec<&UnclearedEntry> = r
                .uncleared
                .iter()
                .filter(|e| g.entry_ids.contains(&e.entry_id))
                .collect();
            lines.push(styled(
                &format!(
                    "{key} = {} ({} items, subtotal {})",
                    g.value,
                    g.count,
                    with_currency(r, g.uncleared_total)
                ),
                bold,
                use_color,
            ));
            lines.push(items_table(&items, mu, use_color));
        }
    } else {
        let items: Vec<&UnclearedEntry> = r.uncleared.iter().collect();
        lines.push(items_table(&items, mu, use_color));
    }
    lines
}

/// Render the report as human-readable tables.
#[must_use]
pub fn render_table(r: &ReconciliationReport, use_color: bool) -> String {
    let green = color(anstyle::AnsiColor::Green);
    let red_bold = color(anstyle::AnsiColor::Red).bold();

    let mut lines = header_lines(r, use_color);
    lines.push(String::new());
    lines.push(balances_table(r, use_color));
    if let Some(ok) = r.is_reconciled() {
        lines.push(if ok {
            styled("[ok] reconciled: yes", green, use_color)
        } else {
            styled("[!!] reconciled: no", red_bold, use_color)
        });
    }
    lines.push(String::new());
    lines.extend(uncleared_lines(r, use_color));
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
    statement: Option<&'a str>,
    metadata_filters: &'a [MetadataFilter],
    group_by_metadata: Option<&'a str>,
    book_balance: i64,
    cleared_balance: i64,
    total_cleared_balance: i64,
    reconciled_balance: i64,
    uncleared_total: i64,
    uncleared_count: usize,
    partially_cleared_count: usize,
    statement_balance: Option<i64>,
    difference: Option<i64>,
    reconciled: Option<bool>,
    uncleared_entries: &'a [UnclearedEntry],
    groups: Option<&'a [GroupTotal]>,
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
        statement: r.statement.as_deref(),
        metadata_filters: &r.filters,
        group_by_metadata: r.group_by.as_deref(),
        book_balance: r.book_balance,
        cleared_balance: r.cleared_balance,
        total_cleared_balance: r.total_cleared_balance,
        reconciled_balance: r.reconciled_balance,
        uncleared_total: r.uncleared_total,
        uncleared_count: r.uncleared.len(),
        partially_cleared_count: r.partially_cleared_count(),
        statement_balance: r.statement_balance,
        difference: r.difference(),
        reconciled: r.is_reconciled(),
        uncleared_entries: &r.uncleared,
        groups: r.groups.as_deref(),
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
/// A single header row, then one `entry` row per outstanding entry, one
/// `group` row per group (with `--group-by-metadata`; `description` = group
/// value, `remaining_amount` = subtotal), followed by `summary` rows whose
/// `description` names the metric (`book_balance`, `cleared_balance`,
/// `total_cleared_balance`, `reconciled_balance`, `uncleared_total`, and,
/// with a statement balance, `statement_balance`, `difference`, `reconciled`
/// with amount `1`/`0`). Amounts are signed minor units on the account's
/// normal side.
///
/// Columns: `row_type`, `entry_id`, `transaction_id`, `date`, `reference`,
/// `description`, `amount`, `cleared_amount`, `remaining_amount`,
/// `currency`, `group`, `metadata`
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
        "reference",
        "description",
        "amount",
        "cleared_amount",
        "remaining_amount",
        "currency",
        "group",
        "metadata",
    ])
    .map_err(csv_err)?;

    for e in &r.uncleared {
        let group = if r.group_by.is_some() {
            e.group.as_deref().unwrap_or(NO_GROUP)
        } else {
            ""
        };
        wtr.write_record([
            "entry",
            &e.entry_id.to_string(),
            &e.transaction_id.to_string(),
            &e.date,
            e.reference.as_deref().unwrap_or(""),
            &e.description,
            &e.signed_amount.to_string(),
            &(e.cleared_amount * e.signed_amount.signum()).to_string(),
            &e.signed_remaining.to_string(),
            ccy,
            group,
            &metadata_cell(e.metadata.as_ref()),
        ])
        .map_err(csv_err)?;
    }

    for g in r.groups.iter().flatten() {
        wtr.write_record([
            "group",
            "",
            "",
            "",
            "",
            &g.value,
            "",
            "",
            &g.uncleared_total.to_string(),
            ccy,
            &g.value,
            "",
        ])
        .map_err(csv_err)?;
    }

    let as_of = r.as_of.as_deref().unwrap_or("");
    let mut summary = vec![
        ("book_balance", r.book_balance),
        ("cleared_balance", r.cleared_balance),
        ("total_cleared_balance", r.total_cleared_balance),
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
        wtr.write_record([
            "summary",
            "",
            "",
            as_of,
            "",
            label,
            &value.to_string(),
            "",
            "",
            ccy,
            "",
            "",
        ])
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

    #[test]
    fn metadata_values_are_lenient_strings() {
        let k = "carrier";
        assert_eq!(
            metadata_value(Some(r#"{"carrier":"Acme"}"#), k).as_deref(),
            Some("Acme")
        );
        assert_eq!(
            metadata_value(Some(r#"{"carrier":42}"#), k).as_deref(),
            Some("42")
        );
        assert_eq!(
            metadata_value(Some(r#"{"carrier":true}"#), k).as_deref(),
            Some("true")
        );
        assert_eq!(metadata_value(Some(r#"{"carrier":null}"#), k), None);
        assert_eq!(metadata_value(Some(r#"{"carrier":{"x":1}}"#), k), None);
        assert_eq!(metadata_value(Some("free text"), k), None);
        assert_eq!(metadata_value(Some("[1,2]"), k), None);
        assert_eq!(metadata_value(None, k), None);
    }
}
