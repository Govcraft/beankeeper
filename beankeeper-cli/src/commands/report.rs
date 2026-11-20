use crate::cli::{AccountTypeArg, Cli, OutputFormat, ReportCommand, resolve_format};
use crate::commands::report_currency::resolve_report_currency;
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
    let db = Db::open_existing(&cli.db, pp.as_ref())?;
    crate::db::companies::get_company(db.conn(), company)?;
    let use_color = output::should_use_color(cli.verbosity.no_color);
    let format = resolve_format(None, cli);

    let resolved = super::report_dates::resolve(sub, chrono::Local::now().date_naive())?;
    let sub = &resolved;

    // `--period` / `--compare` multi-column modes (issue #22).
    let multi = super::report_periods::try_run(cli, &db, company, sub, format, use_color);
    if let Some(result) = multi {
        return result;
    }

    match sub {
        ReportCommand::TrialBalance { .. }
        | ReportCommand::Balance { .. }
        | ReportCommand::IncomeStatement { .. }
        | ReportCommand::BalanceSheet { .. }
        | ReportCommand::TaxSummary { .. } => {
            run_statement_command(cli, &db, company, sub, format, use_color)
        }
        ReportCommand::BudgetVariance { .. } | ReportCommand::Reconciliation { .. } => {
            run_entry_command(cli, &db, company, sub, format, use_color)
        }
        ReportCommand::Ledger { .. } => {
            super::report_ledger::run_command(cli, &db, company, sub, format, use_color)
        }
        ReportCommand::Aging { .. } => {
            super::report_aging::run_command(cli, &db, company, sub, format, use_color)
        }
        ReportCommand::CashFlow(args) => {
            super::report_cash_flow::run(cli, &db, company, args, format, use_color)
        }
    }
}

/// Dispatch the account-balance statements: trial balance, balance, income
/// statement, balance sheet and tax summary.
fn run_statement_command(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    match sub {
        ReportCommand::TrialBalance {
            from,
            to,
            account_type,
            currency,
            ..
        } => run_trial_balance(
            cli,
            db,
            company,
            from.as_deref(),
            to.as_deref(),
            account_type.as_ref(),
            currency.as_deref(),
            format,
            use_color,
        ),
        ReportCommand::Balance {
            account,
            from,
            to,
            currency,
        } => run_balance(
            cli,
            db,
            company,
            account,
            from.as_deref(),
            to.as_deref(),
            currency.as_deref(),
            format,
            use_color,
        ),
        ReportCommand::IncomeStatement {
            from, to, currency, ..
        } => run_income_statement(
            cli,
            db,
            company,
            from.as_deref(),
            to.as_deref(),
            currency.as_deref(),
            format,
            use_color,
        ),
        ReportCommand::BalanceSheet { to, currency } => run_balance_sheet(
            cli,
            db,
            company,
            to.as_deref(),
            currency.as_deref(),
            format,
            use_color,
        ),
        ReportCommand::TaxSummary { from, to, currency } => run_tax_summary(
            cli,
            db,
            company,
            from.as_deref(),
            to.as_deref(),
            currency.as_deref(),
            format,
            use_color,
        ),
        _ => Err(CliError::General(
            "internal error: not a statement report".to_string(),
        )),
    }
}

/// Dispatch budget variance and reconciliation.
fn run_entry_command(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    match sub {
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
            db,
            company,
            *year,
            *month,
            *from,
            *to,
            account_type.as_ref(),
            currency.as_deref(),
            *include_unbudgeted,
            format,
            use_color,
        ),
        ReportCommand::Reconciliation { .. } => {
            super::report_reconciliation::run_command(cli, db, company, sub, format, use_color)
        }
        _ => Err(CliError::General(
            "internal error: not a budget or reconciliation report".to_string(),
        )),
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
    currency: Option<&str>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let currency = resolve_report_currency(db.conn(), company, currency)?;
    let type_filter = account_type.map(|t| format!("{t:?}").to_lowercase());
    let balances = db::compute_trial_balance(
        db.conn(),
        company,
        type_filter.as_deref(),
        from,
        to,
        Some(currency.code()),
    )?;

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
    currency: Option<&str>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    // Look up the account to get name and type
    let account_row = db::get_account(db.conn(), company, account_code)?;
    let currency = resolve_report_currency(db.conn(), company, currency)?;
    let (debit_total, credit_total) = db::compute_account_balance(
        db.conn(),
        company,
        account_code,
        from,
        to,
        Some(currency.code()),
    )?;

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
            let title = build_period_title("Account Balance", currency.code(), from, to);
            let rendered = output::table::render_account_balance(&params);
            print_period_table(&rendered, &title);
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
/// An income statement shows revenue and expense accounts for a given period,
/// grouped into sections with subtotals and a net income (or net loss) line.
#[allow(clippy::too_many_arguments)]
fn run_income_statement(
    cli: &Cli,
    db: &Db,
    company: &str,
    from: Option<&str>,
    to: Option<&str>,
    currency: Option<&str>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let currency = resolve_report_currency(db.conn(), company, currency)?;
    let code = Some(currency.code());
    let revenue_balances = compute_period_balances(db, company, "revenue", from, to, code)?;
    let expense_balances = compute_period_balances(db, company, "expense", from, to, code)?;
    let stmt =
        output::statements::build_income_statement(from, to, &revenue_balances, &expense_balances);

    let minor_units = currency.minor_units();

    match format {
        OutputFormat::Table => {
            let title = build_period_title("Income Statement", currency.code(), from, to);
            let rendered = output::statements::render_income_statement_table(
                &stmt,
                &title,
                minor_units,
                use_color,
            );
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("report.income-statement", Some(company));
            let rendered = output::statements::render_income_statement_json(&stmt, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::statements::render_income_statement_csv(&stmt)?;
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
/// Because there are no closing entries, equity includes a computed retained
/// earnings line (cumulative revenue minus expenses up to `to`) so that
/// Assets = Liabilities + Equity for any valid ledger.
#[allow(clippy::too_many_arguments)]
fn run_balance_sheet(
    cli: &Cli,
    db: &Db,
    company: &str,
    to: Option<&str>,
    currency: Option<&str>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let currency = resolve_report_currency(db.conn(), company, currency)?;
    let code = Some(currency.code());
    let asset_balances =
        db::compute_trial_balance(db.conn(), company, Some("asset"), None, to, code)?;
    let liability_balances =
        db::compute_trial_balance(db.conn(), company, Some("liability"), None, to, code)?;
    let equity_balances =
        db::compute_trial_balance(db.conn(), company, Some("equity"), None, to, code)?;
    let revenue_balances =
        db::compute_trial_balance(db.conn(), company, Some("revenue"), None, to, code)?;
    let expense_balances =
        db::compute_trial_balance(db.conn(), company, Some("expense"), None, to, code)?;

    let sheet = output::statements::build_balance_sheet(&output::statements::BalanceSheetInputs {
        as_of: to,
        assets: &asset_balances,
        liabilities: &liability_balances,
        equity: &equity_balances,
        revenue: &revenue_balances,
        expenses: &expense_balances,
    });

    let minor_units = currency.minor_units();

    match format {
        OutputFormat::Table => {
            let title = build_period_title("Balance Sheet", currency.code(), None, to);
            let rendered = output::statements::render_balance_sheet_table(
                &sheet,
                &title,
                minor_units,
                use_color,
            );
            println!("{rendered}");
        }
        OutputFormat::Json => {
            let meta = output::json::meta("report.balance-sheet", Some(company));
            let rendered = output::statements::render_balance_sheet_json(&sheet, meta)?;
            println!("{rendered}");
        }
        OutputFormat::Csv => {
            let rendered = output::statements::render_balance_sheet_csv(&sheet)?;
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
    currency: Option<&str>,
) -> Result<Vec<db::BalanceRow>, CliError> {
    db::compute_trial_balance(db.conn(), company, Some(type_filter), from, to, currency)
}

/// Replace a renderer's plain heading with its effective date window.
fn print_period_table(rendered: &str, title: &str) {
    let body = rendered.split_once('\n').map_or(rendered, |(_, body)| body);
    println!("{title}\n{body}");
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

/// Execute the `report tax-summary` subcommand.
#[allow(clippy::too_many_arguments)]
fn run_tax_summary(
    cli: &Cli,
    db: &Db,
    company: &str,
    from: Option<&str>,
    to: Option<&str>,
    currency: Option<&str>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let currency = resolve_report_currency(db.conn(), company, currency)?;
    let rows = db::compute_tax_summary(db.conn(), company, from, to, Some(currency.code()))?;
    let minor_units = currency.minor_units();

    match format {
        OutputFormat::Table => {
            let title = build_period_title("Tax Summary", currency.code(), from, to);
            let rendered =
                output::table::render_tax_summary(&rows, currency.code(), minor_units, use_color);
            print_period_table(&rendered, &title);
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
    currency: Option<&str>,
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
    let currency = resolve_report_currency(db.conn(), company, currency)?;
    let currency_code = currency.code();
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

/// Return the three-letter English name for a 1-based month number.
///
/// Out-of-range inputs (outside `1..=12`) fall back to `"?"` rather than
/// panicking on an out-of-bounds index or a sign-losing cast.
fn month_name(month: i32) -> &'static str {
    static MONTH_NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    usize::try_from(month - 1)
        .ok()
        .and_then(|i| MONTH_NAMES.get(i))
        .copied()
        .unwrap_or("?")
}

/// Build a title for the budget-variance report.
fn build_variance_title(currency: &str, year: i32, from_month: i32, to_month: i32) -> String {
    let from_name = month_name(from_month);
    let to_name = month_name(to_month);

    if from_month == to_month {
        format!("Budget vs. Actual -- {year} {from_name} ({currency})")
    } else if from_month == 1 && to_month == 12 {
        format!("Budget vs. Actual -- {year} ({currency})")
    } else {
        format!("Budget vs. Actual -- {year} {from_name}-{to_name} ({currency})")
    }
}
