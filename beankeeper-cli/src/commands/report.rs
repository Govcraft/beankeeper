use std::io::Write;

use beankeeper::types::{AccountType, Currency};
use chrono::{Month, NaiveDate};
use serde::Serialize;

use crate::cli::{AccountTypeArg, Cli, OutputFormat, ReportCommand, resolve_format};
use crate::commands::report_currency::resolve_report_currency;
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;
use crate::output::chart::{self, Chart, WithChart};
use crate::output::json::{Envelope, Meta, serialize_payload};

pub use super::report_dates::DateRange;

/// Run a `bk report` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &str, sub: &ReportCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let mut stdout = std::io::stdout().lock();
    let format = resolve_format(
        match sub {
            ReportCommand::ScheduleC(args) => args.output_format,
            ReportCommand::Vendor1099(args) => args.period.output_format,
            _ => None,
        },
        cli,
    );
    let mut output = ReportOutput::new(
        format,
        output::should_use_color(cli.verbosity.no_color),
        &mut stdout,
    );
    run_to(cli, &db, company, sub, &mut output)
}

/// Output destination and presentation options for a report.
pub(crate) struct ReportOutput<'a> {
    pub format: OutputFormat,
    pub use_color: bool,
    pub writer: &'a mut dyn Write,
    json_payload: Option<&'a mut Option<serde_json::Value>>,
}

impl<'a> ReportOutput<'a> {
    pub(crate) fn new(format: OutputFormat, use_color: bool, writer: &'a mut dyn Write) -> Self {
        Self {
            format,
            use_color,
            writer,
            json_payload: None,
        }
    }

    pub(crate) fn write_json(
        &mut self,
        meta: Meta,
        report: impl Serialize,
        chart: Option<Chart>,
    ) -> Result<(), CliError> {
        let data = WithChart { report, chart };
        if let Some(payload) = self.json_payload.as_deref_mut() {
            *payload = Some(serialize_payload(&data, &meta).map_err(|error| json_error(&error))?);
        } else {
            let envelope = Envelope {
                ok: true,
                meta,
                data: Some(data),
                error: None,
            };
            serde_json::to_writer_pretty(&mut *self.writer, &envelope)
                .map_err(|error| json_error(&error))?;
            writeln!(self.writer)?;
        }
        Ok(())
    }
}

fn json_error(error: &serde_json::Error) -> CliError {
    CliError::General(format!("JSON serialization failed: {error}"))
}

/// Compute a report using the CLI's JSON contract, without writing to stdout.
///
/// The connection is supplied by the caller, so server transports can reuse an
/// already-open database and its startup credentials.
///
/// # Errors
///
/// Returns [`CliError`] for invalid report options or database failures.
pub fn execute_json(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
) -> Result<serde_json::Value, CliError> {
    let mut bytes = Vec::new();
    let mut payload = None;
    let mut output = ReportOutput {
        json_payload: Some(&mut payload),
        ..ReportOutput::new(OutputFormat::Json, false, &mut bytes)
    };
    run_to(cli, db, company, sub, &mut output)?;
    if let Some(payload) = payload {
        return Ok(payload);
    }

    // Reports without a typed JSON output path still use their existing renderer.
    let mut envelope: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| CliError::General(format!("invalid report JSON: {error}")))?;
    envelope
        .get_mut("data")
        .map(serde_json::Value::take)
        .ok_or_else(|| CliError::General("report did not produce a data payload".into()))
}

fn run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    chart::validate_request(cli)?;
    if cli.output.chart {
        // Currency, effective dates, and source rows share the same read snapshot.
        return db::audit::atomically(db.conn(), || run_resolved_to(cli, db, company, sub, output));
    }
    run_resolved_to(cli, db, company, sub, output)
}

fn run_resolved_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    crate::db::companies::get_company(db.conn(), company)?;
    if let Some(result) = super::report_compare::try_run_to(cli, db, company, sub, output) {
        return result;
    }

    if let Some(result) = super::report_periods::try_run_to(cli, db, company, sub, output) {
        return result;
    }

    let today = chrono::Local::now().date_naive();
    let resolved = super::report_dates::resolve(sub, today)?;
    let sub = &resolved;

    match sub {
        ReportCommand::ScheduleC(_) | ReportCommand::Vendor1099(_) => {
            super::report_tax::run_to(cli, db, company, sub, output)
        }
        ReportCommand::Trend(args) => super::report_trend::run_to(cli, db, company, args, output),
        ReportCommand::TrialBalance { .. }
        | ReportCommand::Balance { .. }
        | ReportCommand::IncomeStatement { .. }
        | ReportCommand::BalanceSheet { .. }
        | ReportCommand::TaxSummary { .. } => run_statement_command(cli, db, company, sub, output),
        ReportCommand::BudgetVariance { .. } | ReportCommand::Reconciliation { .. } => {
            run_entry_command(cli, db, company, sub, output)
        }
        ReportCommand::Ledger { .. } => {
            super::report_ledger::run_command_to(cli, db, company, sub, output)
        }
        ReportCommand::Aging { .. } => {
            super::report_aging::run_command_to(cli, db, company, sub, output)
        }
        ReportCommand::CashFlow(args) => {
            super::report_cash_flow::run_to(cli, db, company, args, output)
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
    output: &mut ReportOutput<'_>,
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
            &StatementOptions {
                from: from.as_deref(),
                to: to.as_deref(),
                currency: currency.as_deref(),
                account_type: account_type.as_ref(),
            },
            output,
        ),
        ReportCommand::Balance {
            account,
            from,
            to,
            currency,
            ..
        } => run_balance(
            cli,
            db,
            company,
            account,
            &StatementOptions {
                from: from.as_deref(),
                to: to.as_deref(),
                currency: currency.as_deref(),
                account_type: None,
            },
            output,
        ),
        ReportCommand::IncomeStatement {
            from, to, currency, ..
        } => run_income_statement(
            cli,
            db,
            company,
            &StatementOptions {
                from: from.as_deref(),
                to: to.as_deref(),
                currency: currency.as_deref(),
                account_type: None,
            },
            output,
        ),
        ReportCommand::BalanceSheet { to, currency, .. } => {
            run_balance_sheet(cli, db, company, to.as_deref(), currency.as_deref(), output)
        }
        ReportCommand::TaxSummary { from, to, currency } => run_tax_summary(
            cli,
            db,
            company,
            &StatementOptions {
                from: from.as_deref(),
                to: to.as_deref(),
                currency: currency.as_deref(),
                account_type: None,
            },
            output,
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
    output: &mut ReportOutput<'_>,
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
            &BudgetVarianceOptions {
                year: *year,
                month: *month,
                from: *from,
                to: *to,
                account_type: account_type.as_ref(),
                currency: currency.as_deref(),
                include_unbudgeted: *include_unbudgeted,
            },
            output,
        ),
        ReportCommand::Reconciliation { .. } => {
            super::report_reconciliation::run_command_to(cli, db, company, sub, output)
        }
        _ => Err(CliError::General(
            "internal error: not a budget or reconciliation report".to_string(),
        )),
    }
}

struct StatementOptions<'a> {
    from: Option<&'a str>,
    to: Option<&'a str>,
    currency: Option<&'a str>,
    account_type: Option<&'a AccountTypeArg>,
}

/// Execute the `report trial-balance` subcommand.
fn run_trial_balance(
    cli: &Cli,
    db: &Db,
    company: &str,
    opts: &StatementOptions<'_>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let StatementOptions {
        from,
        to,
        currency,
        account_type,
    } = *opts;
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

    match output.format {
        OutputFormat::Table => {
            let title = build_period_title("Trial Balance", currency.code(), from, to);
            let rendered = output::table::render_trial_balance(
                &balances,
                currency.code(),
                minor_units,
                output.use_color,
                Some(&title),
            )?;
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Json => {
            let meta = cli
                .meta("report.trial-balance", Some(company))
                .with_currency(currency);
            let rendered = output::json::render_trial_balance(&balances, meta)?;
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_trial_balance(&balances)?;
            write!(output.writer, "{rendered}")?;
        }
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] trial balance generated");
    }

    Ok(())
}

/// Execute the `report balance` subcommand.
fn run_balance(
    cli: &Cli,
    db: &Db,
    company: &str,
    account_code: &str,
    opts: &StatementOptions<'_>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let StatementOptions {
        from, to, currency, ..
    } = *opts;
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

    match output.format {
        OutputFormat::Table => {
            let params = output::table::AccountBalanceParams {
                code: &account_row.code,
                name: &account_row.name,
                account_type: &account_row.account_type,
                debit_total,
                credit_total,
                currency_code: currency.code(),
                currency_minor_units: currency.minor_units(),
                use_color: output.use_color,
            };
            let title = build_period_title("Account Balance", currency.code(), from, to);
            let rendered = output::table::render_account_balance(&params);
            print_period_table(&rendered, &title, output.writer)?;
        }
        OutputFormat::Json => {
            let meta = cli
                .meta("report.balance", Some(company))
                .with_currency(currency);
            let rendered =
                output::json::render_account_balance(&balance_row, currency.code(), meta)?;
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_account_balance(&balance_row, currency.code())?;
            write!(output.writer, "{rendered}")?;
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
fn run_income_statement(
    cli: &Cli,
    db: &Db,
    company: &str,
    opts: &StatementOptions<'_>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let StatementOptions {
        from, to, currency, ..
    } = *opts;
    let dates = DateRange::resolve(from, to, chrono::Local::now().date_naive())?;
    let stmt = db::audit::atomically(db.conn(), || {
        let currency = resolve_report_currency(db.conn(), company, currency)?;
        compute_income_statement(db, company, dates, currency)
    })?;

    match output.format {
        OutputFormat::Table => {
            let title = build_period_title(
                "Income Statement",
                stmt.currency().code(),
                Some(&stmt.dates().from().to_string()),
                Some(&stmt.dates().to().to_string()),
            );
            let rendered =
                output::statements::render_income_statement_table(&stmt, &title, output.use_color);
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Json => {
            let meta = cli
                .meta("report.income-statement", Some(company))
                .with_currency(stmt.currency());
            let chart = chart::requested(cli, || {
                chart::income_statement(company, &stmt, cli.output.chart_all)
            })?;
            output.write_json(meta, &stmt, chart)?;
        }
        OutputFormat::Csv => {
            let rendered = output::statements::render_income_statement_csv(&stmt)?;
            write!(output.writer, "{rendered}")?;
        }
    }
    if !cli.verbosity.quiet {
        eprintln!("[ok] income statement generated");
    }
    Ok(())
}

/// Compute an income statement on an already-open ledger using explicit scope.
///
/// # Errors
/// Returns [`CliError`] for an unknown company, database errors or overflow.
pub fn compute_income_statement(
    db: &Db,
    company: &str,
    dates: DateRange,
    currency: Currency,
) -> Result<output::statements::IncomeStatement, CliError> {
    let conn = db.conn();
    db::audit::atomically(conn, || {
        db::companies::get_company(conn, company)?;
        let from = dates.from().to_string();
        let to = dates.to().to_string();
        let load = |kind: AccountType| {
            db::compute_trial_balance(
                conn,
                company,
                Some(kind.code()),
                Some(&from),
                Some(&to),
                Some(currency.code()),
            )
        };
        output::statements::build_income_statement(
            dates,
            currency,
            load(AccountType::Revenue)?,
            load(AccountType::Expense)?,
        )
    })
}

/// Execute the `report balance-sheet` subcommand.
///
/// A balance sheet shows asset, liability, and equity accounts as of a date.
/// Because there are no closing entries, equity includes a computed retained
/// earnings line (cumulative revenue minus expenses up to `to`) so that
/// Assets = Liabilities + Equity for any valid ledger.
fn run_balance_sheet(
    cli: &Cli,
    db: &Db,
    company: &str,
    to: Option<&str>,
    currency: Option<&str>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let as_of = to
        .map(|raw| super::report_dates::parse_date("--to", raw))
        .transpose()?
        .unwrap_or_else(|| chrono::Local::now().date_naive());
    let sheet = db::audit::atomically(db.conn(), || {
        let currency = resolve_report_currency(db.conn(), company, currency)?;
        compute_balance_sheet(db, company, as_of, currency)
    })?;

    match output.format {
        OutputFormat::Table => {
            let title = build_period_title(
                "Balance Sheet",
                sheet.currency().code(),
                None,
                Some(&sheet.as_of().to_string()),
            );
            let rendered =
                output::statements::render_balance_sheet_table(&sheet, &title, output.use_color);
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Json => {
            let meta = cli
                .meta("report.balance-sheet", Some(company))
                .with_currency(sheet.currency());
            output.write_json(meta, &sheet, None)?;
        }
        OutputFormat::Csv => {
            let rendered = output::statements::render_balance_sheet_csv(&sheet)?;
            write!(output.writer, "{rendered}")?;
        }
    }
    if !cli.verbosity.quiet {
        eprintln!("[ok] balance sheet generated");
    }
    Ok(())
}

/// Compute an as-of balance sheet on an already-open ledger using explicit scope.
///
/// # Errors
/// Returns [`CliError`] for an unknown company, database errors or overflow.
pub fn compute_balance_sheet(
    db: &Db,
    company: &str,
    as_of: NaiveDate,
    currency: Currency,
) -> Result<output::statements::BalanceSheet, CliError> {
    let conn = db.conn();
    db::audit::atomically(conn, || {
        db::companies::get_company(conn, company)?;
        let to = as_of.to_string();
        let load = |kind: AccountType| {
            db::compute_trial_balance(
                conn,
                company,
                Some(kind.code()),
                None,
                Some(&to),
                Some(currency.code()),
            )
        };
        output::statements::build_balance_sheet(output::statements::BalanceSheetInputs {
            as_of,
            currency,
            assets: load(AccountType::Asset)?,
            liabilities: load(AccountType::Liability)?,
            equity: load(AccountType::Equity)?,
            revenue: load(AccountType::Revenue)?,
            expenses: load(AccountType::Expense)?,
        })
    })
}

/// Replace a renderer's plain heading with its effective date window.
fn print_period_table(rendered: &str, title: &str, writer: &mut dyn Write) -> Result<(), CliError> {
    let body = rendered.split_once('\n').map_or(rendered, |(_, body)| body);
    writeln!(writer, "{title}\n{body}")?;
    Ok(())
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
fn run_tax_summary(
    cli: &Cli,
    db: &Db,
    company: &str,
    opts: &StatementOptions<'_>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let StatementOptions {
        from, to, currency, ..
    } = *opts;
    let currency = resolve_report_currency(db.conn(), company, currency)?;
    let rows = db::compute_tax_summary(db.conn(), company, from, to, Some(currency.code()))?;
    let minor_units = currency.minor_units();

    match output.format {
        OutputFormat::Table => {
            let title = build_period_title("Tax Summary", currency.code(), from, to);
            let rendered = output::table::render_tax_summary(
                &rows,
                currency.code(),
                minor_units,
                output.use_color,
            );
            print_period_table(&rendered, &title, output.writer)?;
        }
        OutputFormat::Json => {
            let meta = cli
                .meta("report.tax-summary", Some(company))
                .with_currency(currency);
            let rendered = output::json::render_tax_summary(&rows, meta)?;
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_tax_summary(&rows)?;
            write!(output.writer, "{rendered}")?;
        }
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] tax summary generated ({} categories)", rows.len());
    }

    Ok(())
}

struct BudgetVarianceOptions<'a> {
    year: i32,
    month: Option<i32>,
    from: Option<i32>,
    to: Option<i32>,
    account_type: Option<&'a AccountTypeArg>,
    currency: Option<&'a str>,
    include_unbudgeted: bool,
}

/// Execute the `report budget-variance` subcommand.
fn run_budget_variance(
    cli: &Cli,
    db: &Db,
    company: &str,
    opts: &BudgetVarianceOptions<'_>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let BudgetVarianceOptions {
        year,
        month,
        from,
        to,
        account_type,
        currency,
        include_unbudgeted,
    } = *opts;
    let months = budget_months(year, month, from, to)?;
    let account_type = account_type.copied().map(AccountTypeArg::to_library_type);
    let currency = resolve_report_currency(db.conn(), company, currency)?;
    let coverage = if include_unbudgeted {
        db::BudgetCoverage::IncludeUnbudgeted
    } else {
        db::BudgetCoverage::Budgeted
    };
    let report = db::compute_budget_variance(
        db.conn(),
        &db::BudgetVarianceParams {
            company_slug: company,
            currency,
            months,
            account_type,
            coverage,
        },
    )?;

    match output.format {
        OutputFormat::Table => {
            let title = build_variance_title(&report);
            let rendered = output::table::render_budget_variance(&report, &title, output.use_color);
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Json => {
            let meta = cli
                .meta("report.budget-variance", Some(company))
                .with_currency(report.currency());
            output.write_json(meta, &report, None)?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_budget_variance(&report)?;
            write!(output.writer, "{rendered}")?;
        }
    }

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] budget variance generated ({} accounts)",
            report.rows().len()
        );
    }
    Ok(())
}

fn budget_months(
    year: i32,
    month: Option<i32>,
    from: Option<i32>,
    to: Option<i32>,
) -> Result<db::BudgetMonths, CliError> {
    let parse = |month: i32| {
        u8::try_from(month)
            .ok()
            .and_then(|month| Month::try_from(month).ok())
    };
    if let Some(number) = month {
        let month = parse(number)
            .ok_or_else(|| CliError::Validation(format!("month must be 1-12, got {number}")))?;
        return db::BudgetMonths::new(year, month, month);
    }
    let range_error = || CliError::Validation("from/to months must be 1-12".into());
    let from = parse(from.unwrap_or(1)).ok_or_else(range_error)?;
    let to = parse(to.unwrap_or(12)).ok_or_else(range_error)?;
    db::BudgetMonths::new(year, from, to)
}

/// Build a title for the budget-variance report.
fn build_variance_title(report: &db::BudgetVarianceReport) -> String {
    let months = report.months();
    let year = months.year();
    let currency = report.currency();
    let (from, to) = months.dates();
    let from_name = from.format("%b");
    let to_name = to.format("%b");

    if months.from() == months.to() {
        format!("Budget vs. Actual -- {year} {from_name} ({currency})")
    } else if months.from() == Month::January && months.to() == Month::December {
        format!("Budget vs. Actual -- {year} ({currency})")
    } else {
        format!("Budget vs. Actual -- {year} {from_name}-{to_name} ({currency})")
    }
}
