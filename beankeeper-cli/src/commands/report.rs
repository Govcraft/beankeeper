use beankeeper::types::Currency;

use crate::cli::{AccountTypeArg, Cli, OutputFormat, ReportCommand, resolve_format};
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;
use crate::passphrase;

/// Run a `bk report` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &str, sub: &ReportCommand) -> Result<(), CliError> {
    let pp = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open(&cli.db, pp.as_ref())?;
    let use_color = output::should_use_color(cli.verbosity.no_color);
    let format = resolve_format(None, cli);

    match sub {
        ReportCommand::TrialBalance {
            from,
            to,
            account_type,
        } => run_trial_balance(
            cli,
            &db,
            company,
            from.as_deref(),
            to.as_deref(),
            account_type.as_ref(),
            format,
            use_color,
        ),
        ReportCommand::Balance {
            account,
            from,
            to,
        } => run_balance(
            cli,
            &db,
            company,
            account,
            from.as_deref(),
            to.as_deref(),
            format,
            use_color,
        ),
        ReportCommand::IncomeStatement { from, to } => run_income_statement(
            cli,
            &db,
            company,
            from.as_deref(),
            to.as_deref(),
            format,
            use_color,
        ),
        ReportCommand::BalanceSheet { to } => {
            run_balance_sheet(cli, &db, company, to.as_deref(), format, use_color)
        }
        ReportCommand::TaxSummary { from, to } => run_tax_summary(
            cli,
            &db,
            company,
            from.as_deref(),
            to.as_deref(),
            format,
            use_color,
        ),
        ReportCommand::BudgetVariance {
            year,
            month,
            from,
            to,
            account_type,
            currency,
            include_unbudgeted,
        } => run_budget_variance(
            cli,
            &db,
            company,
            *year,
            *month,
            *from,
            *to,
            account_type.as_ref(),
            currency,
            *include_unbudgeted,
            format,
            use_color,
        ),
    }
}

/// Execute the `report trial-balance` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_trial_balance(
    cli: &Cli,
    db: &Db,
    company: &str,
    from: Option<&str>,
    to: Option<&str>,
    account_type: Option<&AccountTypeArg>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let type_filter = account_type.map(|t| format!("{t:?}").to_lowercase());
    let balances =
        db::compute_trial_balance(db.conn(), company, type_filter.as_deref(), from, to)?;

    // Determine currency info (default to USD for display)
    let currency = resolve_company_currency(db, company);
    let minor_units = currency.minor_units();

    match format {
        OutputFormat::Table => {
            let title = build_period_title("Trial Balance", currency.code(), from, to);
            let rendered = output::table::render_trial_balance(
                &balances,
                currency.code(),
                minor_units,
                use_color,
                Some(&title),
            );
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("report.trial-balance", Some(company));
            let rendered = output::json::render_trial_balance(&balances, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_trial_balance(&balances)?;
            print!("{rendered}");
        }
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] trial balance generated");
    }

    Ok(())
}

/// Execute the `report balance` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_balance(
    cli: &Cli,
    db: &Db,
    company: &str,
    account_code: &str,
    from: Option<&str>,
    to: Option<&str>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    // Look up the account to get name and type
    let account_row = db::get_account(db.conn(), company, account_code)?;
    let (debit_total, credit_total) =
        db::compute_account_balance(db.conn(), company, account_code, from, to)?;

    let currency = resolve_company_currency(db, company);

    let balance_row = db::BalanceRow {
        code: account_row.code.clone(),
        name: account_row.name.clone(),
        account_type: account_row.account_type.clone(),
        debit_total,
        credit_total,
    };

    match format {
        OutputFormat::Table => {
            let params = output::table::AccountBalanceParams {
                code: &account_row.code,
                name: &account_row.name,
                account_type: &account_row.account_type,
                debit_total,
                credit_total,
                currency_code: currency.code(),
                currency_minor_units: currency.minor_units(),
                use_color,
            };
            let rendered = output::table::render_account_balance(&params);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("report.balance", Some(company));
            let rendered =
                output::json::render_account_balance(&balance_row, currency.code(), meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_account_balance(&balance_row, currency.code())?;
            print!("{rendered}");
        }
    }

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] balance for account {} ({})",
            account_row.code, account_row.name
        );
    }

    Ok(())
}

/// Execute the `report income-statement` subcommand.
///
/// An income statement shows revenue and expense accounts for a given period.
#[allow(clippy::too_many_arguments)]
fn run_income_statement(
    cli: &Cli,
    db: &Db,
    company: &str,
    from: Option<&str>,
    to: Option<&str>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    // For income statement, we compute trial balance filtered by revenue and
    // expense account types. The date filter uses `to` as the as-of date, but
    // we also need to handle the `from` date for period-based filtering.
    //
    // We'll compute revenue and expense balances separately and combine them.
    let revenue_balances = compute_period_balances(db, company, "revenue", from, to)?;
    let expense_balances = compute_period_balances(db, company, "expense", from, to)?;

    let mut all_balances = Vec::new();
    all_balances.extend(revenue_balances);
    all_balances.extend(expense_balances);

    let currency = resolve_company_currency(db, company);
    let minor_units = currency.minor_units();

    match format {
        OutputFormat::Table => {
            let title = build_period_title("Income Statement", currency.code(), from, to);
            let rendered = render_report_table(
                &title,
                &all_balances,
                minor_units,
                use_color,
                ReportKind::IncomeStatement,
            );
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("report.income-statement", Some(company));
            let rendered = output::json::render_trial_balance(&all_balances, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_trial_balance(&all_balances)?;
            print!("{rendered}");
        }
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] income statement generated");
    }

    Ok(())
}

/// Execute the `report balance-sheet` subcommand.
///
/// A balance sheet shows asset, liability, and equity accounts as of a date.
fn run_balance_sheet(
    cli: &Cli,
    db: &Db,
    company: &str,
    to: Option<&str>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let asset_balances = db::compute_trial_balance(db.conn(), company, Some("asset"), None, to)?;
    let liability_balances =
        db::compute_trial_balance(db.conn(), company, Some("liability"), None, to)?;
    let equity_balances = db::compute_trial_balance(db.conn(), company, Some("equity"), None, to)?;

    let mut all_balances = Vec::new();
    all_balances.extend(asset_balances);
    all_balances.extend(liability_balances);
    all_balances.extend(equity_balances);

    let currency = resolve_company_currency(db, company);
    let minor_units = currency.minor_units();

    match format {
        OutputFormat::Table => {
            let title = build_period_title("Balance Sheet", currency.code(), None, to);
            let rendered = render_report_table(
                &title,
                &all_balances,
                minor_units,
                use_color,
                ReportKind::BalanceCheck,
            );
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("report.balance-sheet", Some(company));
            let rendered = output::json::render_trial_balance(&all_balances, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_trial_balance(&all_balances)?;
            print!("{rendered}");
        }
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] balance sheet generated");
    }

    Ok(())
}

/// Compute period-based balances for a specific account type.
///
/// When a `from` date is provided, we compute the difference between
/// the as-of-`to` balances and the as-of-`from-1-day` balances to get
/// the period activity. When no `from` is given, we just use `to` as
/// the as-of date.
fn compute_period_balances(
    db: &Db,
    company: &str,
    type_filter: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Vec<db::BalanceRow>, CliError> {
    db::compute_trial_balance(db.conn(), company, Some(type_filter), from, to)
}

/// Resolve the currency for a company by checking transactions.
///
/// Defaults to USD if no transactions exist or the currency is unrecognized.
fn resolve_company_currency(db: &Db, company: &str) -> Currency {
    // Try to find the most common currency in the company's transactions
    let result: Result<String, _> = db.conn().query_row(
        "SELECT currency FROM transactions WHERE company_slug = ?1 \
         GROUP BY currency ORDER BY COUNT(*) DESC LIMIT 1",
        rusqlite::params![company],
        |row| row.get(0),
    );

    match result {
        Ok(code) => Currency::from_code(&code).unwrap_or(Currency::USD),
        Err(_) => Currency::USD,
    }
}

/// Build a period-qualified report title.
fn build_period_title(
    base: &str,
    currency_code: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> String {
    match (from, to) {
        (Some(f), Some(t)) => format!("{base} ({f} to {t}) ({currency_code})"),
        (Some(f), None) => format!("{base} (from {f}) ({currency_code})"),
        (None, Some(t)) => format!("{base} (through {t}) ({currency_code})"),
        (None, None) => format!("{base} ({currency_code})"),
    }
}

/// Whether the report checks for balanced debits/credits or shows net income.
#[derive(Clone, Copy)]
enum ReportKind {
    /// Balance sheet / trial balance: debits must equal credits.
    BalanceCheck,
    /// Income statement: the difference is net income (or net loss).
    IncomeStatement,
}

/// Apply ANSI styling to text if colours are enabled.
fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

/// Format minor-unit integers with thousands separators and decimal places.
fn format_amount(minor_units: i64, decimal_places: u8) -> String {
    let abs_units = minor_units.unsigned_abs();
    let divisor = 10u64.pow(u32::from(decimal_places));
    let whole = abs_units / divisor;
    let frac = abs_units % divisor;

    let whole_str = {
        let s = whole.to_string();
        let len = s.len();
        if len <= 3 {
            s
        } else {
            let mut result = String::with_capacity(len + (len - 1) / 3);
            for (i, ch) in s.chars().enumerate() {
                if i > 0 && (len - i) % 3 == 0 {
                    result.push(',');
                }
                result.push(ch);
            }
            result
        }
    };

    let formatted = if decimal_places == 0 {
        whole_str
    } else {
        let frac_str = format!("{frac:0>width$}", width = usize::from(decimal_places));
        format!("{whole_str}.{frac_str}")
    };

    if minor_units < 0 {
        format!("-{formatted}")
    } else {
        formatted
    }
}

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

/// Render a report as a styled table (reuses trial-balance layout with a custom title).
fn render_report_table(
    title: &str,
    balances: &[db::BalanceRow],
    currency_minor_units: u8,
    use_color: bool,
    kind: ReportKind,
) -> String {
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;
    use comfy_table::{Cell, ContentArrangement, Table};

    let bold = anstyle::Style::new().bold();
    let cyan = anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)));
    let green =
        anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)));
    let red_bold = anstyle::Style::new()
        .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)))
        .bold();

    let mut lines = Vec::new();
    lines.push(styled(title, bold, use_color));
    lines.push(String::new());

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);

    table.set_header(vec![
        Cell::new(styled("Code", bold, use_color)),
        Cell::new(styled("Account", bold, use_color)),
        Cell::new(styled("Type", bold, use_color)),
        Cell::new(styled("Debit", bold, use_color)),
        Cell::new(styled("Credit", bold, use_color)),
    ]);

    let (grand_debits, grand_credits) =
        populate_balance_rows(&mut table, balances, currency_minor_units, cyan, use_color);

    append_totals_and_status(
        &mut lines,
        &table,
        grand_debits,
        grand_credits,
        currency_minor_units,
        bold,
        green,
        red_bold,
        use_color,
        kind,
    );

    lines.join("\n")
}

/// Add balance data rows to the table, returning `(total_debits, total_credits)`.
fn populate_balance_rows(
    table: &mut comfy_table::Table,
    balances: &[db::BalanceRow],
    currency_minor_units: u8,
    cyan: anstyle::Style,
    use_color: bool,
) -> (i64, i64) {
    use comfy_table::{Cell, CellAlignment};

    let mut grand_debits: i64 = 0;
    let mut grand_credits: i64 = 0;

    for row in balances {
        grand_debits = grand_debits.saturating_add(row.debit_total);
        grand_credits = grand_credits.saturating_add(row.credit_total);

        table.add_row(vec![
            Cell::new(styled(&row.code, cyan, use_color)),
            Cell::new(&row.name),
            Cell::new(capitalize_first(&row.account_type)),
            Cell::new(format_amount(row.debit_total, currency_minor_units))
                .set_alignment(CellAlignment::Right),
            Cell::new(format_amount(row.credit_total, currency_minor_units))
                .set_alignment(CellAlignment::Right),
        ]);
    }

    (grand_debits, grand_credits)
}

/// Append the totals row and status to the output lines.
///
/// For `BalanceCheck` reports (balance sheet), debits must equal credits.
/// For `IncomeStatement` reports, the difference is net income or net loss.
#[allow(clippy::too_many_arguments)]
fn append_totals_and_status(
    lines: &mut Vec<String>,
    table: &comfy_table::Table,
    grand_debits: i64,
    grand_credits: i64,
    currency_minor_units: u8,
    bold: anstyle::Style,
    green: anstyle::Style,
    red_bold: anstyle::Style,
    use_color: bool,
    kind: ReportKind,
) {
    lines.push(table.to_string());
    lines.push(String::new());

    let debit_str = format_amount(grand_debits, currency_minor_units);
    let credit_str = format_amount(grand_credits, currency_minor_units);
    lines.push(format!(
        "Totals:  DR {dr}  CR {cr}",
        dr = styled(&debit_str, bold, use_color),
        cr = styled(&credit_str, bold, use_color),
    ));

    match kind {
        ReportKind::BalanceCheck => {
            if grand_debits == grand_credits {
                lines.push(styled("[ok] BALANCED", green, use_color));
            } else {
                let diff = grand_debits.saturating_sub(grand_credits).abs();
                let diff_str = format_amount(diff, currency_minor_units);
                lines.push(styled(
                    &format!("[!!] UNBALANCED (difference: {diff_str})"),
                    red_bold,
                    use_color,
                ));
            }
        }
        ReportKind::IncomeStatement => {
            // Revenue is credit-normal, expenses are debit-normal.
            // Net income = credits (revenue) - debits (expenses).
            let net = grand_credits.saturating_sub(grand_debits);
            let net_str = format_amount(net.abs(), currency_minor_units);
            if net >= 0 {
                lines.push(styled(&format!("Net Income: {net_str}"), green, use_color));
            } else {
                lines.push(styled(&format!("Net Loss: {net_str}"), red_bold, use_color));
            }
        }
    }
}

/// Execute the `report tax-summary` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_tax_summary(
    cli: &Cli,
    db: &Db,
    company: &str,
    from: Option<&str>,
    to: Option<&str>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let rows = db::compute_tax_summary(db.conn(), company, from, to)?;
    let currency = resolve_company_currency(db, company);
    let minor_units = currency.minor_units();

    match format {
        OutputFormat::Table => {
            let rendered =
                output::table::render_tax_summary(&rows, currency.code(), minor_units, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("report.tax-summary", Some(company));
            let rendered = output::json::render_tax_summary(&rows, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_tax_summary(&rows)?;
            print!("{rendered}");
        }
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] tax summary generated ({} categories)", rows.len());
    }

    Ok(())
}

/// Execute the `report budget-variance` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_budget_variance(
    cli: &Cli,
    db: &Db,
    company: &str,
    year: i32,
    month: Option<i32>,
    from: Option<i32>,
    to: Option<i32>,
    account_type: Option<&AccountTypeArg>,
    currency_code: &str,
    include_unbudgeted: bool,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    // Resolve month range
    let (from_month, to_month) = if let Some(m) = month {
        if !(1..=12).contains(&m) {
            return Err(CliError::Validation(format!("month must be 1-12, got {m}")));
        }
        (m, m)
    } else {
        let f = from.unwrap_or(1);
        let t = to.unwrap_or(12);
        if !(1..=12).contains(&f) || !(1..=12).contains(&t) {
            return Err(CliError::Validation(
                "from/to months must be 1-12".to_string(),
            ));
        }
        if f > t {
            return Err(CliError::Validation(format!(
                "from month ({f}) must be <= to month ({t})"
            )));
        }
        (f, t)
    };

    let type_filter = account_type.map(|t| format!("{t:?}").to_lowercase());
    let currency = Currency::from_code(currency_code)
        .map_err(|_| CliError::Validation(format!("unknown currency: {currency_code}")))?;
    let minor_units = currency.minor_units();

    let rows = db::compute_budget_variance(
        db.conn(),
        &db::BudgetVarianceParams {
            company_slug: company,
            currency: currency_code,
            year,
            from_month,
            to_month,
            account_type: type_filter.as_deref(),
            include_unbudgeted,
        },
    )?;

    match format {
        OutputFormat::Table => {
            let title = build_variance_title(currency_code, year, from_month, to_month);
            let rendered =
                output::table::render_budget_variance(&rows, &title, minor_units, use_color);
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("report.budget-variance", Some(company));
            let rendered = output::json::render_budget_variance(
                &rows,
                year,
                from_month,
                to_month,
                currency_code,
                meta,
            )?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_budget_variance(&rows)?;
            print!("{rendered}");
        }
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] budget variance generated ({} accounts)", rows.len());
    }

    Ok(())
}

/// Build a title for the budget-variance report.
fn build_variance_title(currency: &str, year: i32, from_month: i32, to_month: i32) -> String {
    static MONTH_NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let from_name = MONTH_NAMES[(from_month - 1) as usize];
    let to_name = MONTH_NAMES[(to_month - 1) as usize];

    if from_month == to_month {
        format!("Budget vs. Actual -- {year} {from_name} ({currency})")
    } else if from_month == 1 && to_month == 12 {
        format!("Budget vs. Actual -- {year} ({currency})")
    } else {
        format!("Budget vs. Actual -- {year} {from_name}-{to_name} ({currency})")
    }
}
