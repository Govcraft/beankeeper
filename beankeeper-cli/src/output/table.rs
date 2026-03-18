//! Human-readable table output using `comfy-table` and `anstyle` colours.

use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};

use crate::db::{
    AccountRow, AccountWithBalanceRow, AttachmentRow, BalanceRow, CompanyRow, EntryRow,
    TransactionRow,
};

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
fn format_amount(minor_units: i64, decimal_places: u8) -> String {
    let negative = minor_units < 0;
    let abs_units = minor_units.unsigned_abs();
    let divisor = 10u64.pow(u32::from(decimal_places));

    let whole = abs_units / divisor;
    let frac = abs_units % divisor;

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
fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let len = s.len();

    if len <= 3 {
        return s;
    }

    let mut result = String::with_capacity(len + (len - 1) / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

// ---------------------------------------------------------------------------
// Styling helpers
// ---------------------------------------------------------------------------

/// Derive the normal-balance label from a lowercase account-type string.
fn normal_balance_label(account_type: &str) -> &'static str {
    match account_type {
        "asset" | "expense" => "Debit",
        "liability" | "equity" | "revenue" => "Credit",
        _ => "Unknown",
    }
}

/// Wrap `text` in ANSI escape codes if colours are enabled.
fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

fn cyan_style() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)))
}

fn green_style() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)))
}

fn red_bold_style() -> anstyle::Style {
    anstyle::Style::new()
        .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)))
        .bold()
}

fn bold_style() -> anstyle::Style {
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

    let count = companies.len();
    format!(
        "{table}\n\n{count} {noun}",
        noun = if count == 1 { "company" } else { "companies" }
    )
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
            Cell::new(capitalize_first(&a.account_type)),
            Cell::new(normal_balance_label(&a.account_type)),
        ]);
    }

    let count = accounts.len();
    format!(
        "{table}\n\n{count} {noun}",
        noun = if count == 1 { "account" } else { "accounts" }
    )
}

/// Render a list of accounts with balance totals as a table.
#[must_use]
pub fn render_accounts_with_balances(
    accounts: &[AccountWithBalanceRow],
    use_color: bool,
) -> String {
    if accounts.is_empty() {
        return "No accounts found.".to_string();
    }

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Code", bold_style(), use_color)),
        Cell::new(styled("Name", bold_style(), use_color)),
        Cell::new(styled("Type", bold_style(), use_color)),
        Cell::new(styled("Debit Total", bold_style(), use_color)),
        Cell::new(styled("Credit Total", bold_style(), use_color)),
    ]);

    for a in accounts {
        table.add_row(vec![
            Cell::new(styled(&a.code, cyan_style(), use_color)),
            Cell::new(&a.name),
            Cell::new(capitalize_first(&a.account_type)),
            Cell::new(a.debit_total).set_alignment(CellAlignment::Right),
            Cell::new(a.credit_total).set_alignment(CellAlignment::Right),
        ]);
    }

    let count = accounts.len();
    format!(
        "{table}\n\n{count} {noun}",
        noun = if count == 1 { "account" } else { "accounts" }
    )
}

/// Render a list of transactions as a summary table.
#[must_use]
pub fn render_transaction_list(transactions: &[TransactionRow], use_color: bool) -> String {
    if transactions.is_empty() {
        return "No transactions found.".to_string();
    }

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("ID", bold_style(), use_color)),
        Cell::new(styled("Date", bold_style(), use_color)),
        Cell::new(styled("Description", bold_style(), use_color)),
        Cell::new(styled("Currency", bold_style(), use_color)),
    ]);

    for txn in transactions {
        table.add_row(vec![
            Cell::new(txn.id).set_alignment(CellAlignment::Right),
            Cell::new(&txn.date),
            Cell::new(&txn.description),
            Cell::new(&txn.currency),
        ]);
    }

    let count = transactions.len();
    format!(
        "{table}\n\n{count} {noun}",
        noun = if count == 1 {
            "transaction"
        } else {
            "transactions"
        }
    )
}

/// Render a single transaction in detail with its entry lines.
#[must_use]
pub fn render_transaction_detail(
    txn: &TransactionRow,
    entries: &[EntryRow],
    currency_minor_units: u8,
    use_color: bool,
) -> String {
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
    lines.push(String::new());

    // Entries
    lines.push(styled("  Entries:", bold_style(), use_color));

    let mut total_debits: i64 = 0;
    let mut total_credits: i64 = 0;

    for entry in entries {
        let amount_str = format_amount(entry.amount, currency_minor_units);

        let (prefix, direction_style) = if entry.direction == "debit" {
            total_debits = total_debits.saturating_add(entry.amount);
            ("DR", cyan_style())
        } else {
            total_credits = total_credits.saturating_add(entry.amount);
            ("CR", dim_style())
        };

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
        lines.push(format!(
            "    {prefix} {code}  {amount_str}{memo_suffix}{tax_suffix}",
            prefix = styled(prefix, direction_style, use_color),
            code = styled(&entry.account_code, cyan_style(), use_color),
        ));
    }

    lines.push(String::new());

    // Totals
    let debit_str = format_amount(total_debits, currency_minor_units);
    let credit_str = format_amount(total_credits, currency_minor_units);
    lines.push(format!("  Total Debits:  {debit_str}"));
    lines.push(format!("  Total Credits: {credit_str}"));

    if total_debits == total_credits {
        lines.push(styled("  [ok] BALANCED", green_style(), use_color));
    } else {
        lines.push(styled("  [!!] UNBALANCED", red_bold_style(), use_color));
    }

    lines.join("\n")
}

/// Render a trial balance report as a table with totals and balance status.
#[must_use]
pub fn render_trial_balance(
    balances: &[BalanceRow],
    currency_code: &str,
    currency_minor_units: u8,
    use_color: bool,
) -> String {
    let mut lines = Vec::new();
    lines.push(styled(
        &format!("Trial Balance ({currency_code})"),
        bold_style(),
        use_color,
    ));
    lines.push(String::new());

    let mut table = new_table();
    table.set_header(vec![
        Cell::new(styled("Code", bold_style(), use_color)),
        Cell::new(styled("Account", bold_style(), use_color)),
        Cell::new(styled("Debit", bold_style(), use_color)),
        Cell::new(styled("Credit", bold_style(), use_color)),
    ]);

    let mut grand_debits: i64 = 0;
    let mut grand_credits: i64 = 0;

    for row in balances {
        grand_debits = grand_debits.saturating_add(row.debit_total);
        grand_credits = grand_credits.saturating_add(row.credit_total);

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
            &format_amount(grand_debits, currency_minor_units),
            bold_style(),
            use_color,
        ))
        .set_alignment(CellAlignment::Right),
        Cell::new(styled(
            &format_amount(grand_credits, currency_minor_units),
            bold_style(),
            use_color,
        ))
        .set_alignment(CellAlignment::Right),
    ]);

    lines.push(table.to_string());
    lines.push(String::new());

    if grand_debits == grand_credits {
        lines.push(styled("[ok] BALANCED", green_style(), use_color));
    } else {
        let diff = grand_debits.saturating_sub(grand_credits).abs();
        let diff_str = format_amount(diff, currency_minor_units);
        lines.push(styled(
            &format!("[!!] UNBALANCED (difference: {diff_str})"),
            red_bold_style(),
            use_color,
        ));
    }

    lines.join("\n")
}

/// Parameters for rendering an account balance.
pub struct AccountBalanceParams<'a> {
    /// Account code.
    pub code: &'a str,
    /// Account name.
    pub name: &'a str,
    /// Account type (lowercase, e.g. "asset").
    pub account_type: &'a str,
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
    lines.push(format!("  Type:    {}", capitalize_first(account_type)));
    lines.push(format!("  Normal:  {}", normal_balance_label(account_type)));
    lines.push(String::new());

    let debit_str = format_amount(debit_total, currency_minor_units);
    let credit_str = format_amount(credit_total, currency_minor_units);

    lines.push(format!("  Debits:  {debit_str} {currency_code}"));
    lines.push(format!("  Credits: {credit_str} {currency_code}"));

    // Net balance: debit-normal accounts use (debits - credits),
    // credit-normal accounts use (credits - debits).
    let net = match account_type {
        "asset" | "expense" => debit_total.saturating_sub(credit_total),
        _ => credit_total.saturating_sub(debit_total),
    };
    let net_str = format_amount(net, currency_minor_units);
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
fn new_table() -> Table {
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
            .as_deref()
            .map(|h| {
                let short = &h[..h.len().min(12)];
                format!("  ({short})")
            })
            .unwrap_or_default();

        let filename = att.original_filename.as_deref().unwrap_or("(no filename)");

        lines.push(format!(
            "    [{doc_type}] {filename}{hash_suffix}",
            doc_type = styled(&att.document_type, cyan_style(), use_color),
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
        let net = row.debit_total.saturating_sub(row.credit_total);
        table.add_row(vec![
            Cell::new(styled(&row.tax_category, cyan_style(), use_color)),
            Cell::new(format_amount(row.debit_total, currency_minor_units))
                .set_alignment(CellAlignment::Right),
            Cell::new(format_amount(row.credit_total, currency_minor_units))
                .set_alignment(CellAlignment::Right),
            Cell::new(format_amount(net, currency_minor_units)).set_alignment(CellAlignment::Right),
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- format_amount --

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

    // -- normal_balance_label --

    #[test]
    fn normal_balance_debit_types() {
        assert_eq!(normal_balance_label("asset"), "Debit");
        assert_eq!(normal_balance_label("expense"), "Debit");
    }

    #[test]
    fn normal_balance_credit_types() {
        assert_eq!(normal_balance_label("liability"), "Credit");
        assert_eq!(normal_balance_label("equity"), "Credit");
        assert_eq!(normal_balance_label("revenue"), "Credit");
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
        assert!(out.contains("1 company"));
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
        assert!(out.contains("2 companies"));
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
            account_type: "asset".into(),
            created_at: "2025-01-01".into(),
            default_tax_category: None,
        }];
        let out = render_accounts(&rows, false);
        assert!(out.contains("1000"));
        assert!(out.contains("Cash"));
        assert!(out.contains("Asset"));
        assert!(out.contains("Debit"));
        assert!(out.contains("1 account"));
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
            reference: None,
        }];
        let out = render_transaction_list(&rows, false);
        assert!(out.contains('1'));
        assert!(out.contains("2025-03-15"));
        assert!(out.contains("Test sale"));
        assert!(out.contains("1 transaction"));
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
            reference: None,
        };
        let entries = vec![
            EntryRow {
                id: 1,
                transaction_id: 42,
                account_code: "1000".into(),
                company_slug: "acme".into(),
                direction: "debit".into(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
            EntryRow {
                id: 2,
                transaction_id: 42,
                account_code: "4000".into(),
                company_slug: "acme".into(),
                direction: "credit".into(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
        ];
        let out = render_transaction_detail(&txn, &entries, 2, false);
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
            reference: None,
        };
        let entries = vec![
            EntryRow {
                id: 1,
                transaction_id: 1,
                account_code: "1000".into(),
                company_slug: "acme".into(),
                direction: "debit".into(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
            EntryRow {
                id: 2,
                transaction_id: 1,
                account_code: "4000".into(),
                company_slug: "acme".into(),
                direction: "credit".into(),
                amount: 3000,
                memo: None,
                tax_category: None,
            },
        ];
        let out = render_transaction_detail(&txn, &entries, 2, false);
        assert!(out.contains("[!!] UNBALANCED"));
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
        let out = render_trial_balance(&rows, "USD", 2, false);
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
        let out = render_trial_balance(&rows, "USD", 2, false);
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
        let out = render_trial_balance(&rows, "JPY", 0, false);
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
            account_type: "asset",
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
            account_type: "liability",
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
