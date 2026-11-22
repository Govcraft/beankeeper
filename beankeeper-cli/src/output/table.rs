//! Human-readable table output using `comfy-table` and `anstyle` colours.

use beankeeper::types::{Amount, DebitOrCredit};
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};

use std::collections::HashMap;

use super::amounts::format_money_grouped;
use super::{TrialBalanceTotals, trial_balance_overflow};
use crate::db::clearings::{Clearing, cleared_sum};
use crate::db::{
    AccountRow, AccountWithBalanceRow, AttachmentRow, BalanceRow, BudgetRow, BudgetVarianceReport,
    CompanyRow, EntryRow, EntryStatus, TransactionRow,
};
use crate::error::CliError;

// ---------------------------------------------------------------------------
// Amount formatting
// ---------------------------------------------------------------------------

/// Format a minor-unit integer as a human-readable string with thousands
/// separators and the appropriate number of decimal places.
///
/// # Examples
///
/// - `format_amount(123456, 2)` => `"1,234.56"`
/// - `format_amount(5000, 0)` => `"5,000"`
/// - `format_amount(1234567, 3)` => `"1,234.567"`
/// - `format_amount(0, 2)` => `"0.00"`
pub(crate) fn format_amount(minor_units: i64, decimal_places: u8) -> String {
    format_wide_amount(i128::from(minor_units), decimal_places)
}

fn format_wide_amount(minor_units: i128, decimal_places: u8) -> String {
    let negative = minor_units < 0;
    let abs_units = minor_units.unsigned_abs();
    let (whole, frac) = 10u128
        .checked_pow(u32::from(decimal_places))
        .map_or((0, abs_units), |divisor| {
            (abs_units / divisor, abs_units % divisor)
        });

    let whole_str = format_with_commas(whole);

    let formatted = if decimal_places == 0 {
        whole_str
    } else {
        let frac_str = format!("{frac:0>width$}", width = usize::from(decimal_places));
        format!("{whole_str}.{frac_str}")
    };

    if negative {
        format!("-{formatted}")
    } else {
        formatted
    }
}

/// Insert thousands-separator commas into a non-negative integer.
fn format_with_commas(n: u128) -> String {
    let s = n.to_string();
    let len = s.len();

    if len <= 3 {
        return s;
    }

    let mut result = String::with_capacity(len + (len - 1) / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

// ---------------------------------------------------------------------------
// Styling helpers
// ---------------------------------------------------------------------------

/// Wrap `text` in ANSI escape codes if colours are enabled.
pub(super) fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

pub(super) fn cyan_style() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)))
}

pub(super) fn green_style() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)))
}

pub(super) fn red_bold_style() -> anstyle::Style {
    anstyle::Style::new()
        .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)))
        .bold()
}

pub(super) fn bold_style() -> anstyle::Style {
    anstyle::Style::new().bold()
}

fn dim_style() -> anstyle::Style {
    anstyle::Style::new().dimmed()
}

// ---------------------------------------------------------------------------
// Rendering functions
// ---------------------------------------------------------------------------

/// Render a list of companies as a table.
#[must_use]
pub fn render_companies(companies: &[CompanyRow], use_color: bool) -> String {
    if companies.is_empty() {
        return "No companies found.".to_string();
    }

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Slug", bold_style(), use_color)),
        Cell::new(styled("Name", bold_style(), use_color)),
        Cell::new(styled("Description", bold_style(), use_color)),
        Cell::new(styled("Created", bold_style(), use_color)),
    ]);

    for c in companies {
        table.add_row(vec![
            Cell::new(styled(&c.slug, cyan_style(), use_color)),
            Cell::new(&c.name),
            Cell::new(c.description.as_deref().unwrap_or("")),
            Cell::new(styled(&c.created_at, dim_style(), use_color)),
        ]);
    }

    table.to_string()
}

/// Render a list of accounts as a table.
#[must_use]
pub fn render_accounts(accounts: &[AccountRow], use_color: bool) -> String {
    if accounts.is_empty() {
        return "No accounts found.".to_string();
    }

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Code", bold_style(), use_color)),
        Cell::new(styled("Name", bold_style(), use_color)),
        Cell::new(styled("Type", bold_style(), use_color)),
        Cell::new(styled("Normal Balance", bold_style(), use_color)),
    ]);

    for a in accounts {
        table.add_row(vec![
            Cell::new(styled(&a.code, cyan_style(), use_color)),
            Cell::new(&a.name),
            Cell::new(a.account_type.to_string()),
            Cell::new(a.account_type.normal_balance().to_string()),
        ]);
    }

    table.to_string()
}

/// Render a list of accounts with balance totals as a table.
#[must_use]
pub fn render_accounts_with_balances(
    accounts: &[AccountWithBalanceRow],
    use_color: bool,
) -> String {
    let Some(first) = accounts.first() else {
        return "No accounts found.".to_string();
    };
    let currency = first.debit_total.currency().code();

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Code", bold_style(), use_color)),
        Cell::new(styled("Name", bold_style(), use_color)),
        Cell::new(styled("Type", bold_style(), use_color)),
        Cell::new(styled(
            &format!("Debit Total ({currency})"),
            bold_style(),
            use_color,
        )),
        Cell::new(styled(
            &format!("Credit Total ({currency})"),
            bold_style(),
            use_color,
        )),
    ]);

    for a in accounts {
        table.add_row(vec![
            Cell::new(styled(&a.code, cyan_style(), use_color)),
            Cell::new(&a.name),
            Cell::new(capitalize_first(&a.account_type)),
            Cell::new(format_money_grouped(a.debit_total)).set_alignment(CellAlignment::Right),
            Cell::new(format_money_grouped(a.credit_total)).set_alignment(CellAlignment::Right),
        ]);
    }

    table.to_string()
}

/// Render a list of transactions as a summary table.
#[must_use]
pub fn render_transaction_list(transactions: &[TransactionRow], use_color: bool) -> String {
    if transactions.is_empty() {
        return "No transactions found.".to_string();
    }

    let links: Vec<String> = transactions.iter().map(reversal_links).collect();
    let has_links = links.iter().any(|link| !link.is_empty());
    let mut table = new_table();
    let mut header = vec![
        Cell::new(styled("ID", bold_style(), use_color)),
        Cell::new(styled("Date", bold_style(), use_color)),
        Cell::new(styled("Description", bold_style(), use_color)),
        Cell::new(styled("Currency", bold_style(), use_color)),
    ];
    if has_links {
        header.push(Cell::new(styled("Reversal", bold_style(), use_color)));
    }
    table.set_header(header);
    for (txn, link) in transactions.iter().zip(links) {
        let mut row = vec![
            Cell::new(txn.id).set_alignment(CellAlignment::Right),
            Cell::new(&txn.date),
            Cell::new(&txn.description),
            Cell::new(&txn.currency),
        ];
        if has_links {
            row.push(Cell::new(link));
        }
        table.add_row(row);
    }

    table.to_string()
}

/// Render only valid numeric reversal links, tolerating legacy metadata.
fn reversal_links(txn: &TransactionRow) -> String {
    let metadata: Option<serde_json::Value> = txn
        .metadata
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok());
    let mut links = Vec::new();
    if let Some(meta) = metadata {
        for (key, label) in [("reverses", "Reverses"), ("reversed_by", "Reversed by")] {
            if let Some(id) = meta.get(key).and_then(serde_json::Value::as_i64) {
                links.push(format!("{label} #{id}"));
            }
        }
    }
    links.join(", ")
}

/// Render a single transaction in detail with its entry lines.
///
/// # Errors
/// Returns a validation error if entry totals exceed the supported amount.
pub fn render_transaction_detail(
    txn: &TransactionRow,
    entries: &[EntryRow],
    currency_minor_units: u8,
    use_color: bool,
) -> Result<String, CliError> {
    render_transaction_detail_with_clearings::<std::hash::RandomState>(
        txn,
        entries,
        None,
        currency_minor_units,
        use_color,
    )
}

/// Render a single transaction in detail. With `clearings` (entry id to its
/// clearings), each entry line is followed by its id, cleared and remaining
/// amounts, and one line per clearing.
///
/// # Errors
/// Returns a validation error if entry totals exceed the supported amount.
pub fn render_transaction_detail_with_clearings<S: ::std::hash::BuildHasher>(
    txn: &TransactionRow,
    entries: &[EntryRow],
    clearings: Option<&HashMap<i64, Vec<Clearing>, S>>,
    currency_minor_units: u8,
    use_color: bool,
) -> Result<String, CliError> {
    let mut lines = Vec::new();

    // Header
    lines.push(styled("Transaction Detail", bold_style(), use_color));
    lines.push(format!(
        "  ID:          {}",
        styled(&txn.id.to_string(), cyan_style(), use_color)
    ));
    lines.push(format!("  Date:        {}", txn.date));
    lines.push(format!("  Description: {}", txn.description));
    lines.push(format!("  Currency:    {}", txn.currency));
    let reversal = reversal_links(txn);
    if !reversal.is_empty() {
        lines.push(format!("  Reversal:    {reversal}"));
    }

    if let Some(ref meta) = txn.metadata {
        lines.push(format!(
            "  Metadata:    {}",
            styled(meta, dim_style(), use_color)
        ));
    }

    if let Some(ref reference) = txn.reference {
        lines.push(format!(
            "  Reference:   {}",
            styled(reference, dim_style(), use_color)
        ));
    }

    lines.push(format!(
        "  Posted at:   {}",
        styled(&txn.posted_at, dim_style(), use_color)
    ));
    lines.push(format!(
        "  Posted by:   {}",
        styled(
            txn.posted_by.as_deref().unwrap_or("unknown (legacy)"),
            dim_style(),
            use_color
        )
    ));
    if let Some(uri) = &txn.agent_uri {
        lines.push(format!("  Agent:       {uri}"));
        lines.push(format!(
            "  Token:       {}",
            txn.token_jti.as_deref().unwrap_or("unknown")
        ));
    }
    lines.push(String::new());

    // Entries
    lines.push(styled("  Entries:", bold_style(), use_color));

    let mut total_debits = Amount::ZERO;
    let mut total_credits = Amount::ZERO;

    for entry in entries {
        let amount_str = format_amount(entry.amount, currency_minor_units);
        let (total, prefix, direction_style) = match entry.direction {
            DebitOrCredit::Debit => (&mut total_debits, "DR", cyan_style()),
            DebitOrCredit::Credit => (&mut total_credits, "CR", dim_style()),
        };
        *total = total.checked_add(entry.amount.into()).ok_or_else(|| {
            CliError::Validation("transaction total exceeds the supported amount".into())
        })?;

        let memo_suffix = entry
            .memo
            .as_deref()
            .map(|m| format!("  ({m})"))
            .unwrap_or_default();
        let tax_suffix = entry
            .tax_category
            .as_deref()
            .map(|c| format!("  [{c}]"))
            .unwrap_or_default();
        let status_suffix = match entry.status {
            EntryStatus::Cleared => format!("  {}", styled("*C*", green_style(), use_color)),
            EntryStatus::Reconciled => format!("  {}", styled("*R*", green_style(), use_color)),
            EntryStatus::Uncleared => String::new(),
        };

        lines.push(format!(
            "    {prefix} {code}  {amount_str}{memo_suffix}{tax_suffix}{status_suffix}",
            prefix = styled(prefix, direction_style, use_color),
            code = styled(&entry.account_code, cyan_style(), use_color),
        ));

        if let Some(map) = clearings {
            let list = map.get(&entry.id).map_or(&[][..], Vec::as_slice);
            lines.extend(clearing_lines(entry, list, currency_minor_units, use_color));
        }
    }

    lines.push(String::new());

    // Totals
    let debit_str = format_wide_amount(total_debits.minor_units(), currency_minor_units);
    let credit_str = format_wide_amount(total_credits.minor_units(), currency_minor_units);
    lines.push(format!("  Total Debits:  {debit_str}"));
    lines.push(format!("  Total Credits: {credit_str}"));

    if total_debits == total_credits {
        lines.push(styled("  [ok] BALANCED", green_style(), use_color));
    } else {
        lines.push(styled("  [!!] UNBALANCED", red_bold_style(), use_color));
    }

    Ok(lines.join("\n"))
}

/// The `txn show` lines under an entry: its id with cleared and remaining
/// amounts, then one line per clearing.
fn clearing_lines(
    entry: &EntryRow,
    clearings: &[Clearing],
    currency_minor_units: u8,
    use_color: bool,
) -> Vec<String> {
    let cleared = cleared_sum(clearings);
    let mut out = vec![styled(
        &format!(
            "       entry #{}: cleared {} of {}, remaining {}",
            entry.id,
            format_amount(cleared, currency_minor_units),
            format_amount(entry.amount, currency_minor_units),
            format_amount(entry.amount - cleared, currency_minor_units),
        ),
        dim_style(),
        use_color,
    )];
    for c in clearings {
        let reconciled = c
            .state()
            .reconciled_on()
            .map(|d| format!("  reconciled {d}"))
            .unwrap_or_default();
        let statement = c
            .statement_ref
            .as_deref()
            .map(|s| format!("  statement {s}"))
            .unwrap_or_default();
        out.push(format!(
            "         clearing #{id}  {amount}  {status}  cleared {date}{reconciled}{statement}",
            id = c.id,
            amount = format_amount(c.amount, currency_minor_units),
            status = c.state().status().as_str(),
            date = c.cleared_on(),
        ));
    }
    out
}

/// Render a trial balance report as a table with totals and balance status.
/// # Errors
/// Returns a validation error when a total cannot be represented.
pub fn render_trial_balance(
    balances: &[BalanceRow],
    currency_code: &str,
    currency_minor_units: u8,
    use_color: bool,
    title_override: Option<&str>,
) -> Result<String, CliError> {
    let TrialBalanceTotals { debits, credits } = TrialBalanceTotals::from_rows(balances)?;
    let mut lines = Vec::new();
    let default_title = format!("Trial Balance ({currency_code})");
    let title = title_override.unwrap_or(&default_title);
    lines.push(styled(title, bold_style(), use_color));
    lines.push(String::new());

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Code", bold_style(), use_color)),
        Cell::new(styled("Account", bold_style(), use_color)),
        Cell::new(styled("Debit", bold_style(), use_color)),
        Cell::new(styled("Credit", bold_style(), use_color)),
    ]);

    for row in balances {
        table.add_row(vec![
            Cell::new(styled(&row.code, cyan_style(), use_color)),
            Cell::new(&row.name),
            Cell::new(format_amount(row.debit_total, currency_minor_units))
                .set_alignment(CellAlignment::Right),
            Cell::new(format_amount(row.credit_total, currency_minor_units))
                .set_alignment(CellAlignment::Right),
        ]);
    }

    // Totals row
    table.add_row(vec![
        Cell::new(""),
        Cell::new(styled("Totals", bold_style(), use_color)),
        Cell::new(styled(
            &format_amount(debits, currency_minor_units),
            bold_style(),
            use_color,
        ))
        .set_alignment(CellAlignment::Right),
        Cell::new(styled(
            &format_amount(credits, currency_minor_units),
            bold_style(),
            use_color,
        ))
        .set_alignment(CellAlignment::Right),
    ]);

    lines.push(table.to_string());
    lines.push(String::new());

    if debits == credits {
        lines.push(styled("[ok] BALANCED", green_style(), use_color));
    } else {
        let diff = i64::try_from(debits.abs_diff(credits)).map_err(|_| trial_balance_overflow())?;
        let diff_str = format_amount(diff, currency_minor_units);
        lines.push(styled(
            &format!("[!!] UNBALANCED (difference: {diff_str})"),
            red_bold_style(),
            use_color,
        ));
    }

    Ok(lines.join("\n"))
}

/// Parameters for rendering an account balance.
pub struct AccountBalanceParams<'a> {
    /// Account code.
    pub code: &'a str,
    /// Account name.
    pub name: &'a str,
    /// Account type (lowercase, e.g. "asset").
    pub account_type: beankeeper::types::AccountType,
    /// Total debits in minor units.
    pub debit_total: i64,
    /// Total credits in minor units.
    pub credit_total: i64,
    /// Currency code (e.g. "USD").
    pub currency_code: &'a str,
    /// Number of decimal places for the currency.
    pub currency_minor_units: u8,
    /// Whether to emit ANSI colour codes.
    pub use_color: bool,
}

/// Render a single account's balance summary.
#[must_use]
pub fn render_account_balance(p: &AccountBalanceParams<'_>) -> String {
    let code = p.code;
    let name = p.name;
    let account_type = p.account_type;
    let debit_total = p.debit_total;
    let credit_total = p.credit_total;
    let currency_code = p.currency_code;
    let currency_minor_units = p.currency_minor_units;
    let use_color = p.use_color;
    let mut lines = Vec::new();

    lines.push(styled("Account Balance", bold_style(), use_color));
    lines.push(format!(
        "  Code:    {}",
        styled(code, cyan_style(), use_color)
    ));
    lines.push(format!("  Name:    {name}"));
    lines.push(format!("  Type:    {account_type}"));
    lines.push(format!("  Normal:  {}", account_type.normal_balance()));
    lines.push(String::new());

    let debit_str = format_amount(debit_total, currency_minor_units);
    let credit_str = format_amount(credit_total, currency_minor_units);

    lines.push(format!("  Debits:  {debit_str} {currency_code}"));
    lines.push(format!("  Credits: {credit_str} {currency_code}"));

    // Net balance: debit-normal accounts use (debits - credits),
    // credit-normal accounts use (credits - debits).
    let net = if account_type.is_debit_normal() {
        i128::from(debit_total) - i128::from(credit_total)
    } else {
        i128::from(credit_total) - i128::from(debit_total)
    };
    let net_str = format_wide_amount(net, currency_minor_units);
    lines.push(format!("  Balance: {net_str} {currency_code}"));

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Capitalise the first letter of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            let upper: String = first.to_uppercase().collect();
            format!("{upper}{rest}", rest = chars.as_str())
        }
    }
}

/// Create a new `comfy_table::Table` with consistent styling.
pub(super) fn new_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

/// Render a list of attachments as a section below transaction details.
#[must_use]
pub fn render_attachments(attachments: &[AttachmentRow], use_color: bool) -> String {
    let mut lines = Vec::new();
    lines.push(styled("  Attachments:", bold_style(), use_color));

    for att in attachments {
        let hash_suffix = att
            .hash
            .as_ref()
            .map(|hash| format!("  ({})", hash.short()))
            .unwrap_or_default();

        let filename = att.original_filename.as_deref().unwrap_or("(no filename)");

        lines.push(format!(
            "    [{doc_type}] {filename}{hash_suffix}",
            doc_type = styled(att.document_type.as_str(), cyan_style(), use_color),
        ));
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Orphaned correlations
// ---------------------------------------------------------------------------

/// Render orphaned intercompany correlations as a table.
#[must_use]
pub fn render_orphaned_correlations(
    orphans: &[crate::db::OrphanedCorrelation],
    _use_color: bool,
) -> String {
    use comfy_table::{ContentArrangement, Table};

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        "Txn ID",
        "Company",
        "Date",
        "Description",
        "Partner ID",
    ]);

    for o in orphans {
        table.add_row(vec![
            o.transaction_id.to_string(),
            o.company_slug.clone(),
            o.date.clone(),
            o.description.clone(),
            o.partner_id.to_string(),
        ]);
    }

    format!("{table}\n\n{} orphaned correlation(s)", orphans.len())
}

// ---------------------------------------------------------------------------
// Tax summary
// ---------------------------------------------------------------------------

/// Render a tax summary report as a table.
#[must_use]
pub fn render_tax_summary(
    rows: &[crate::db::TaxSummaryRow],
    currency_code: &str,
    currency_minor_units: u8,
    use_color: bool,
) -> String {
    if rows.is_empty() {
        return "No categorised entries found.".to_string();
    }

    let mut lines = Vec::new();
    lines.push(styled(
        &format!("Tax Summary ({currency_code})"),
        bold_style(),
        use_color,
    ));
    lines.push(String::new());

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Tax Category", bold_style(), use_color)),
        Cell::new(styled("Debit Total", bold_style(), use_color)),
        Cell::new(styled("Credit Total", bold_style(), use_color)),
        Cell::new(styled("Net", bold_style(), use_color)),
    ]);

    for row in rows {
        let net = i128::from(row.debit_total) - i128::from(row.credit_total);
        table.add_row(vec![
            Cell::new(styled(&row.tax_category, cyan_style(), use_color)),
            Cell::new(format_amount(row.debit_total, currency_minor_units))
                .set_alignment(CellAlignment::Right),
            Cell::new(format_amount(row.credit_total, currency_minor_units))
                .set_alignment(CellAlignment::Right),
            Cell::new(format_wide_amount(net, currency_minor_units))
                .set_alignment(CellAlignment::Right),
        ]);
    }

    lines.push(table.to_string());

    let count = rows.len();
    lines.push(format!(
        "\n{count} tax {noun}",
        noun = if count == 1 { "category" } else { "categories" }
    ));

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Budget tables
// ---------------------------------------------------------------------------

static MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Render budget rows as a table.
///
/// # Errors
/// Returns [`CliError`] if a row contains an invalid stored currency.
pub fn render_budgets(rows: &[BudgetRow], use_color: bool) -> Result<String, CliError> {
    if rows.is_empty() {
        return Ok("No budgets found.".to_string());
    }

    let mut lines = Vec::new();
    lines.push(styled("Budgets", bold_style(), use_color));
    lines.push(String::new());

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Account", bold_style(), use_color)),
        Cell::new(styled("Year", bold_style(), use_color)),
        Cell::new(styled("Month", bold_style(), use_color)),
        Cell::new(styled("Amount", bold_style(), use_color)),
        Cell::new(styled("Currency", bold_style(), use_color)),
        Cell::new(styled("Notes", bold_style(), use_color)),
    ]);

    for row in rows {
        let month_idx = usize::try_from(row.month - 1).unwrap_or(0);
        let month_name = MONTH_NAMES.get(month_idx).unwrap_or(&"?");
        table.add_row(vec![
            Cell::new(styled(&row.account_code, cyan_style(), use_color)),
            Cell::new(row.year.to_string()),
            Cell::new(format!("{} ({})", month_name, row.month)),
            Cell::new(format_money_grouped(row.money()?)).set_alignment(CellAlignment::Right),
            Cell::new(&row.currency),
            Cell::new(row.notes.as_deref().unwrap_or("")),
        ]);
    }

    lines.push(table.to_string());

    Ok(lines.join("\n"))
}

/// Render a budget-variance report as a table.
#[must_use]
pub fn render_budget_variance(
    report: &BudgetVarianceReport,
    title: &str,
    use_color: bool,
) -> String {
    if report.rows().is_empty() {
        return "No budget data found for this period.".to_string();
    }

    let mut lines = Vec::new();
    lines.push(styled(title, bold_style(), use_color));
    lines.push(String::new());

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Code", bold_style(), use_color)),
        Cell::new(styled("Account", bold_style(), use_color)),
        Cell::new(styled("Type", bold_style(), use_color)),
        Cell::new(styled("Budget", bold_style(), use_color)),
        Cell::new(styled("Actual", bold_style(), use_color)),
        Cell::new(styled("Variance", bold_style(), use_color)),
        Cell::new(styled("%", bold_style(), use_color)),
        Cell::new(styled("Status", bold_style(), use_color)),
    ]);

    for row in report.rows() {
        let pct_str = match row.variance_percent {
            Some(p) => format!("{}%", p.to_fixed(1)),
            None => "--".to_string(),
        };

        let on_budget = row.variance_amount.amount().minor_units() == 0;
        let status = if on_budget {
            "ON BUDGET"
        } else if row.favorable {
            "FAV"
        } else {
            "UNFAV"
        };

        let status_style = if on_budget {
            bold_style()
        } else if row.favorable {
            green_style()
        } else {
            red_bold_style()
        };

        table.add_row(vec![
            Cell::new(styled(&row.code, cyan_style(), use_color)),
            Cell::new(&row.name),
            Cell::new(row.account_type.to_string()),
            Cell::new(format_money_grouped(row.budget_amount)).set_alignment(CellAlignment::Right),
            Cell::new(format_money_grouped(row.actual_amount)).set_alignment(CellAlignment::Right),
            Cell::new(format_money_grouped(row.variance_amount))
                .set_alignment(CellAlignment::Right),
            Cell::new(&pct_str).set_alignment(CellAlignment::Right),
            Cell::new(styled(status, status_style, use_color)),
        ]);
    }

    lines.push(table.to_string());
    lines.push(String::new());

    let budget_str = format_money_grouped(report.total_budget());
    let actual_str = format_money_grouped(report.total_actual());
    lines.push(format!(
        "Totals:  Budget {b}  Actual {a}",
        b = styled(&budget_str, bold_style(), use_color),
        a = styled(&actual_str, bold_style(), use_color),
    ));

    lines.join("\n")
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- format_amount --

    #[test]
    fn budgets_use_each_rows_currency() -> Result<(), CliError> {
        let rows = ["USD", "JPY", "BHD"].map(|currency| BudgetRow {
            id: 1,
            company_slug: "acme".into(),
            account_code: "5000".into(),
            currency: currency.into(),
            year: 2026,
            month: 1,
            amount: 1234,
            notes: None,
            created_at: String::new(),
        });
        let table = render_budgets(&rows, false)?;
        for amount in ["12.34", "1,234", "1.234"] {
            assert!(table.contains(amount), "{table}");
        }
        Ok(())
    }

    #[test]
    fn format_amount_cents() {
        assert_eq!(format_amount(12345, 2), "123.45");
    }

    #[test]
    fn format_amount_zero_cents() {
        assert_eq!(format_amount(0, 2), "0.00");
    }

    #[test]
    fn format_amount_no_decimals() {
        assert_eq!(format_amount(5000, 0), "5,000");
    }

    #[test]
    fn format_amount_three_decimals() {
        assert_eq!(format_amount(1_234_567, 3), "1,234.567");
    }

    #[test]
    fn format_amount_with_commas() {
        assert_eq!(format_amount(123_456_789, 2), "1,234,567.89");
    }

    #[test]
    fn format_amount_negative() {
        assert_eq!(format_amount(-5000, 2), "-50.00");
    }

    #[test]
    fn format_amount_small() {
        assert_eq!(format_amount(1, 2), "0.01");
    }

    // -- format_with_commas --

    #[test]
    fn commas_small_numbers() {
        assert_eq!(format_with_commas(0), "0");
        assert_eq!(format_with_commas(1), "1");
        assert_eq!(format_with_commas(999), "999");
    }

    #[test]
    fn commas_thousands() {
        assert_eq!(format_with_commas(1000), "1,000");
        assert_eq!(format_with_commas(1_000_000), "1,000,000");
    }

    // -- capitalize_first --

    #[test]
    fn capitalize_first_works() {
        assert_eq!(capitalize_first("asset"), "Asset");
        assert_eq!(capitalize_first(""), "");
        assert_eq!(capitalize_first("a"), "A");
    }

    // -- render_companies --

    #[test]
    fn render_companies_empty() {
        assert_eq!(render_companies(&[], false), "No companies found.");
    }

    #[test]
    fn render_companies_single() {
        let rows = vec![CompanyRow {
            slug: "acme".into(),
            name: "Acme Corp".into(),
            description: None,
            created_at: "2025-01-01 00:00:00".into(),
        }];
        let out = render_companies(&rows, false);
        assert!(out.contains("acme"));
        assert!(out.contains("Acme Corp"));
        assert!(!out.contains("1 company"));
    }

    #[test]
    fn render_companies_plural() {
        let rows = vec![
            CompanyRow {
                slug: "acme".into(),
                name: "Acme Corp".into(),
                description: None,
                created_at: "2025-01-01".into(),
            },
            CompanyRow {
                slug: "globex".into(),
                name: "Globex Inc".into(),
                description: Some("Global exports".into()),
                created_at: "2025-02-01".into(),
            },
        ];
        let out = render_companies(&rows, false);
        assert!(!out.contains("2 companies"));
    }

    // -- render_accounts --

    #[test]
    fn render_accounts_empty() {
        assert_eq!(render_accounts(&[], false), "No accounts found.");
    }

    #[test]
    fn render_accounts_shows_normal_balance() {
        let rows = vec![AccountRow {
            company_slug: "acme".into(),
            code: "1000".into(),
            name: "Cash".into(),
            account_type: beankeeper::types::AccountType::Asset,
            created_at: "2025-01-01".into(),
            default_tax_category: None,
        }];
        let out = render_accounts(&rows, false);
        assert!(out.contains("1000"));
        assert!(out.contains("Cash"));
        assert!(out.contains("Asset"));
        assert!(out.contains("Debit"));
        assert!(!out.contains("1 account"));
    }

    // -- render_transaction_list --

    #[test]
    fn render_transaction_list_empty() {
        assert_eq!(
            render_transaction_list(&[], false),
            "No transactions found."
        );
    }

    #[test]
    fn render_transaction_list_shows_id_and_date() {
        let rows = vec![TransactionRow {
            id: 1,
            company_slug: "acme".into(),
            description: "Test sale".into(),
            metadata: None,
            currency: "USD".into(),
            date: "2025-03-15".into(),
            posted_at: "2025-03-15 10:00:00".into(),
            posted_by: None,
            agent_uri: None,
            token_jti: None,
            reference: None,
        }];
        let out = render_transaction_list(&rows, false);
        assert!(out.contains('1'));
        assert!(out.contains("2025-03-15"));
        assert!(out.contains("Test sale"));
        assert!(!out.contains("1 transaction"));
    }

    // -- render_transaction_detail --

    #[test]
    fn render_transaction_detail_balanced() {
        let txn = TransactionRow {
            id: 42,
            company_slug: "acme".into(),
            description: "Cash sale".into(),
            metadata: Some("INV-001".into()),
            currency: "USD".into(),
            date: "2025-03-15".into(),
            posted_at: "2025-03-15 10:00:00".into(),
            posted_by: None,
            agent_uri: None,
            token_jti: None,
            reference: None,
        };
        let entries = vec![
            EntryRow {
                id: 1,
                transaction_id: 42,
                account_code: "1000".into(),
                company_slug: "acme".into(),
                direction: DebitOrCredit::Debit,
                amount: 5000,
                memo: None,
                tax_category: None,
                status: EntryStatus::Uncleared,
            },
            EntryRow {
                id: 2,
                transaction_id: 42,
                account_code: "4000".into(),
                company_slug: "acme".into(),
                direction: DebitOrCredit::Credit,
                amount: 5000,
                memo: None,
                tax_category: None,
                status: EntryStatus::Uncleared,
            },
        ];
        let out =
            render_transaction_detail(&txn, &entries, 2, false).expect("representable totals");
        assert!(out.contains("42"));
        assert!(out.contains("Cash sale"));
        assert!(out.contains("INV-001"));
        assert!(out.contains("DR"));
        assert!(out.contains("CR"));
        assert!(out.contains("50.00"));
        assert!(out.contains("[ok] BALANCED"));
    }

    #[test]
    fn render_transaction_detail_unbalanced() {
        let txn = TransactionRow {
            id: 1,
            company_slug: "acme".into(),
            description: "Bad".into(),
            metadata: None,
            currency: "USD".into(),
            date: "2025-03-15".into(),
            posted_at: "2025-03-15 10:00:00".into(),
            posted_by: None,
            agent_uri: None,
            token_jti: None,
            reference: None,
        };
        let entries = vec![
            EntryRow {
                id: 1,
                transaction_id: 1,
                account_code: "1000".into(),
                company_slug: "acme".into(),
                direction: DebitOrCredit::Debit,
                amount: 5000,
                memo: None,
                tax_category: None,
                status: EntryStatus::Uncleared,
            },
            EntryRow {
                id: 2,
                transaction_id: 1,
                account_code: "4000".into(),
                company_slug: "acme".into(),
                direction: DebitOrCredit::Credit,
                amount: 3000,
                memo: None,
                tax_category: None,
                status: EntryStatus::Uncleared,
            },
        ];
        let out =
            render_transaction_detail(&txn, &entries, 2, false).expect("representable totals");
        assert!(out.contains("[!!] UNBALANCED"));
        let debit = EntryRow {
            amount: i64::MAX,
            ..entries[0].clone()
        };
        let credit = EntryRow {
            amount: i64::MAX,
            ..entries[1].clone()
        };
        let wide = render_transaction_detail(&txn, &[debit.clone(), debit, credit], 2, false)
            .expect("totals fit the wide Amount type");
        assert!(wide.contains("Total Debits:  184,467,440,737,095,516.14"));
        assert!(wide.contains("Total Credits: 92,233,720,368,547,758.07"));
        assert!(wide.contains("[!!] UNBALANCED"));
    }

    // -- render_trial_balance --

    #[test]
    fn render_trial_balance_balanced() {
        let rows = vec![
            BalanceRow {
                code: "1000".into(),
                name: "Cash".into(),
                account_type: "asset".into(),
                debit_total: 10000,
                credit_total: 0,
            },
            BalanceRow {
                code: "4000".into(),
                name: "Revenue".into(),
                account_type: "revenue".into(),
                debit_total: 0,
                credit_total: 10000,
            },
        ];
        let out = render_trial_balance(&rows, "USD", 2, false, None)
            .expect("test totals are representable");
        assert!(out.contains("Trial Balance (USD)"));
        assert!(out.contains("1000"));
        assert!(out.contains("Cash"));
        assert!(out.contains("100.00"));
        assert!(out.contains("[ok] BALANCED"));
    }

    #[test]
    fn render_trial_balance_unbalanced() {
        let rows = vec![BalanceRow {
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            debit_total: 10000,
            credit_total: 5000,
        }];
        let out = render_trial_balance(&rows, "USD", 2, false, None)
            .expect("test totals are representable");
        assert!(out.contains("[!!] UNBALANCED"));
    }

    #[test]
    fn render_trial_balance_jpy() {
        let rows = vec![BalanceRow {
            code: "1000".into(),
            name: "Cash".into(),
            account_type: "asset".into(),
            debit_total: 5000,
            credit_total: 5000,
        }];
        let out = render_trial_balance(&rows, "JPY", 0, false, None)
            .expect("test totals are representable");
        assert!(out.contains("Trial Balance (JPY)"));
        assert!(out.contains("5,000"));
        assert!(out.contains("[ok] BALANCED"));
    }

    // -- render_account_balance --

    #[test]
    fn render_account_balance_asset() {
        let p = AccountBalanceParams {
            code: "1000",
            name: "Cash",
            account_type: beankeeper::types::AccountType::Asset,
            debit_total: 15000,
            credit_total: 5000,
            currency_code: "USD",
            currency_minor_units: 2,
            use_color: false,
        };
        let out = render_account_balance(&p);
        assert!(out.contains("1000"));
        assert!(out.contains("Cash"));
        assert!(out.contains("Asset"));
        assert!(out.contains("Debit"));
        assert!(out.contains("150.00")); // debits
        assert!(out.contains("50.00")); // credits
        assert!(out.contains("100.00")); // net = 150 - 50
    }

    #[test]
    fn render_account_balance_liability() {
        let p = AccountBalanceParams {
            code: "2000",
            name: "Loans",
            account_type: beankeeper::types::AccountType::Liability,
            debit_total: 0,
            credit_total: 50000,
            currency_code: "USD",
            currency_minor_units: 2,
            use_color: false,
        };
        let out = render_account_balance(&p);
        assert!(out.contains("2000"));
        assert!(out.contains("Loans"));
        assert!(out.contains("Credit"));
        assert!(out.contains("500.00")); // net = credits - debits
    }
}
