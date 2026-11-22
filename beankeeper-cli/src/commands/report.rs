use crate::operation::Operation;
use std::io::Write;

use beankeeper::types::{AccountType, Currency};
use chrono::{Month, NaiveDate};
use serde::Serialize;

use crate::cli::{
    BudgetVarianceArgs, Cli, CurrencyArg, OutputFormat, ReportCommand, ReportMonth, resolve_format,
};
use crate::commands::report_currency::resolve_report_currency;
use crate::commands::report_tax::{self, ResolvedTaxReport, TaxInvocation};
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;
use crate::output::chart::{self, Chart, WithChart};
use crate::output::json::{Envelope, Meta};

pub use super::report_dates::DateRange;
use super::report_dates::DateWindow;

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
        if let Some(chart) = chart {
            self.write_data(
                meta,
                WithChart {
                    report,
                    chart: Some(chart),
                },
            )
        } else {
            self.write_data(meta, report)
        }
    }

    fn write_data(&mut self, meta: Meta, data: impl Serialize) -> Result<(), CliError> {
        if let Some(payload) = self.json_payload.as_deref_mut() {
            *payload = Some(serde_json::to_value(&data).map_err(|error| json_error(&error))?);
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
    let mut writer = std::io::sink();
    let mut payload = None;
    let mut output = ReportOutput {
        json_payload: Some(&mut payload),
        ..ReportOutput::new(OutputFormat::Json, false, &mut writer)
    };
    run_to(cli, db, company, sub, &mut output)?;
    payload.ok_or_else(|| CliError::General("report did not produce a data payload".into()))
}

fn run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    chart::validate_request(cli)?;
    db::audit::atomically(db.conn(), || run_resolved_to(cli, db, company, sub, output))
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
    ResolvedReport::resolve(db, company, sub, today)?.run(cli, db, company, output)
}

enum ResolvedReport<'a> {
    Tax(ResolvedTaxReport),
    Trend(super::report_trend::TrendRequest),
    TrialBalance(StatementWindow, Option<AccountType>),
    Balance(StatementWindow, db::AccountRow),
    IncomeStatement(DateRange, Currency),
    BalanceSheet(NaiveDate, Currency),
    TaxSummary(DateRange, Currency),
    BudgetVariance(BudgetVarianceOptions),
    Reconciliation(super::report_reconciliation::ResolvedReconciliation<'a>),
    Ledger(super::report_ledger::LedgerRequest<'a>),
    Aging(super::report_aging::ResolvedAging<'a>),
    CashFlow(super::report_cash_flow::CashFlowRequest<'a>),
}

struct StatementWindow {
    dates: DateWindow,
    currency: Currency,
}

impl<'a> ResolvedReport<'a> {
    fn resolve(
        db: &Db,
        company: &str,
        sub: &'a ReportCommand,
        today: NaiveDate,
    ) -> Result<Self, CliError> {
        let currency_for = |currency| {
            resolve_report_currency(db.conn(), company, CurrencyArg::resolve_optional(currency)?)
        };
        let window = |from: Option<&String>, to: Option<&String>| {
            DateWindow::resolve(from.map(String::as_str), to.map(String::as_str), today)
        };
        let range = |from: Option<&String>, to: Option<&String>| {
            DateRange::resolve(from.map(String::as_str), to.map(String::as_str), today)
        };
        Ok(match sub {
            ReportCommand::ScheduleC(args) => {
                Self::Tax(TaxInvocation::ScheduleC(args).resolve(db, company, today)?)
            }
            ReportCommand::Vendor1099(args) => {
                Self::Tax(TaxInvocation::Vendor1099(args).resolve(db, company, today)?)
            }
            ReportCommand::Trend(args) => Self::Trend(args.resolve(db, company, today)?),
            ReportCommand::TrialBalance {
                from,
                to,
                account_type,
                currency,
                ..
            } => Self::TrialBalance(
                StatementWindow {
                    dates: window(from.as_ref(), to.as_ref())?,
                    currency: currency_for(currency.as_ref())?,
                },
                *account_type,
            ),
            ReportCommand::Balance {
                account,
                from,
                to,
                currency,
                ..
            } => Self::resolve_balance(
                db,
                company,
                account,
                window(from.as_ref(), to.as_ref())?,
                currency.as_ref(),
            )?,
            ReportCommand::IncomeStatement {
                from, to, currency, ..
            } => Self::IncomeStatement(
                range(from.as_ref(), to.as_ref())?,
                currency_for(currency.as_ref())?,
            ),
            ReportCommand::BalanceSheet { to, currency, .. } => Self::BalanceSheet(
                window(None, to.as_ref())?.to,
                currency_for(currency.as_ref())?,
            ),
            ReportCommand::TaxSummary { from, to, currency } => Self::TaxSummary(
                range(from.as_ref(), to.as_ref())?,
                currency_for(currency.as_ref())?,
            ),
            ReportCommand::BudgetVariance(args) => {
                Self::BudgetVariance(BudgetVarianceOptions::resolve(db, company, args)?)
            }
            ReportCommand::Reconciliation(args) => Self::Reconciliation(
                super::report_reconciliation::ResolvedReconciliation::resolve(
                    db, company, args, today,
                )?,
            ),
            ReportCommand::Ledger {
                account,
                from,
                to,
                currency,
                filter_metadata,
            } => Self::Ledger(
                super::report_ledger::LedgerOptions {
                    accounts: account,
                    from: from.as_deref(),
                    to: to.as_deref(),
                    currency: CurrencyArg::resolve_optional(currency.as_ref())?,
                    filter_metadata,
                }
                .resolve_cli(db, company, today)?,
            ),
            ReportCommand::Aging(args) => Self::Aging(
                super::report_aging::ResolvedAging::from_command(db, company, args, today)?,
            ),
            ReportCommand::CashFlow(args) => Self::CashFlow(args.resolve(db, company, today)?),
        })
    }

    fn resolve_balance(
        db: &Db,
        company: &str,
        account: &str,
        dates: DateWindow,
        currency: Option<&CurrencyArg>,
    ) -> Result<Self, CliError> {
        let account = db::get_account(db.conn(), company, account)?;
        let currency =
            resolve_report_currency(db.conn(), company, CurrencyArg::resolve_optional(currency)?)?;
        Ok(Self::Balance(StatementWindow { dates, currency }, account))
    }

    fn run(
        self,
        cli: &Cli,
        db: &Db,
        company: &str,
        output: &mut ReportOutput<'_>,
    ) -> Result<(), CliError> {
        match self {
            Self::Tax(tax) => report_tax::run_to(cli, db, company, tax, output),
            Self::Trend(args) => super::report_trend::run_to(cli, db, company, args, output),
            Self::TrialBalance(window, account_type) => {
                run_trial_balance(cli, db, company, &window, account_type, output)
            }
            Self::Balance(window, account) => {
                run_balance(cli, db, company, &account, &window, output)
            }
            Self::IncomeStatement(dates, currency) => {
                run_income_statement(cli, db, company, dates, currency, output)
            }
            Self::BalanceSheet(as_of, currency) => {
                run_balance_sheet(cli, db, company, as_of, currency, output)
            }
            Self::TaxSummary(dates, currency) => {
                run_tax_summary(cli, db, company, dates, currency, output)
            }
            Self::BudgetVariance(options) => run_budget_variance(cli, db, company, options, output),
            Self::Reconciliation(request) => {
                super::report_reconciliation::run_resolved_to(cli, db, company, request, output)
            }
            Self::Ledger(args) => super::report_ledger::run_to(cli, db, company, args, output),
            Self::Aging(request) => {
                super::report_aging::run_resolved_to(cli, db, company, request, output)
            }
            Self::CashFlow(args) => super::report_cash_flow::run_to(cli, db, company, args, output),
        }
    }
}

/// Execute the `report trial-balance` subcommand.
fn run_trial_balance(
    cli: &Cli,
    db: &Db,
    company: &str,
    window: &StatementWindow,
    account_type: Option<AccountType>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let currency = window.currency;
    let from = window.dates.from.map(|date| date.to_string());
    let to = window.dates.to.to_string();
    let (from, to) = (from.as_deref(), Some(to.as_str()));
    let balances = db::compute_trial_balance(db.conn(), company, account_type, from, to, currency)?;

    match output.format {
        OutputFormat::Table => {
            let title = build_period_title("Trial Balance", currency.code(), from, to);
            let rendered = output::table::render_trial_balance(
                &balances,
                currency,
                output.use_color,
                Some(&title),
            )?;
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Json => {
            let meta = cli
                .meta(Operation::ReportTrialBalance, Some(company))
                .with_currency(currency);
            output.write_json(
                meta,
                output::json::trial_balance_data(&balances, currency)?,
                None,
            )?;
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
    account_row: &db::AccountRow,
    window: &StatementWindow,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let currency = window.currency;
    let from = window.dates.from.map(|date| date.to_string());
    let to = window.dates.to.to_string();
    let (from, to) = (from.as_deref(), Some(to.as_str()));
    let (debit_total, credit_total) =
        db::compute_account_balance(db.conn(), company, &account_row.code, from, to, currency)?;

    let balance_row = db::BalanceRow {
        code: account_row.code.clone(),
        name: account_row.name.clone(),
        account_type: account_row.account_type,
        debit_total,
        credit_total,
    };

    match output.format {
        OutputFormat::Table => {
            let params = output::table::AccountBalanceParams {
                code: &account_row.code,
                name: &account_row.name,
                account_type: account_row.account_type,
                debit_total,
                credit_total,
                currency,
                use_color: output.use_color,
            };
            let title = build_period_title("Account Balance", currency.code(), from, to);
            let rendered = output::table::render_account_balance(&params);
            print_period_table(&rendered, &title, output.writer)?;
        }
        OutputFormat::Json => {
            let meta = cli
                .meta(Operation::ReportBalance, Some(company))
                .with_currency(currency);
            output.write_json(
                meta,
                output::json::account_balance_data(&balance_row, currency),
                None,
            )?;
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
    dates: DateRange,
    currency: Currency,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let stmt = compute_income_statement(db, company, dates, currency)?;

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
                .meta(Operation::ReportIncomeStatement, Some(company))
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
            db::compute_trial_balance(conn, company, Some(kind), Some(&from), Some(&to), currency)
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
    as_of: NaiveDate,
    currency: Currency,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let sheet = compute_balance_sheet(db, company, as_of, currency)?;

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
                .meta(Operation::ReportBalanceSheet, Some(company))
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
            db::compute_trial_balance(conn, company, Some(kind), None, Some(&to), currency)
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
    dates: DateRange,
    currency: Currency,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let (from, to) = (dates.from().to_string(), dates.to().to_string());
    let (from, to) = (Some(from.as_str()), Some(to.as_str()));
    let rows = db::compute_tax_summary(db.conn(), company, from, to, currency)?;

    match output.format {
        OutputFormat::Table => {
            let title = build_period_title("Tax Summary", currency.code(), from, to);
            let rendered = output::table::render_tax_summary(&rows, currency, output.use_color);
            print_period_table(&rendered, &title, output.writer)?;
        }
        OutputFormat::Json => {
            let meta = cli
                .meta(Operation::ReportTaxSummary, Some(company))
                .with_currency(currency);
            output.write_json(meta, output::json::tax_summary_data(&rows, currency), None)?;
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

#[derive(Clone, Copy)]
struct BudgetVarianceOptions {
    months: db::BudgetMonths,
    account_type: Option<AccountType>,
    currency: Currency,
    coverage: db::BudgetCoverage,
}

impl BudgetVarianceOptions {
    fn resolve(db: &Db, company: &str, args: &BudgetVarianceArgs) -> Result<Self, CliError> {
        let coverage = if args.include_unbudgeted {
            db::BudgetCoverage::IncludeUnbudgeted
        } else {
            db::BudgetCoverage::Budgeted
        };
        Ok(Self {
            months: budget_months(args.year, args.month, args.from, args.to)?,
            account_type: args.account_type,
            currency: resolve_report_currency(
                db.conn(),
                company,
                CurrencyArg::resolve_optional(args.currency.as_ref())?,
            )?,
            coverage,
        })
    }
}

/// Execute the `report budget-variance` subcommand.
fn run_budget_variance(
    cli: &Cli,
    db: &Db,
    company: &str,
    opts: BudgetVarianceOptions,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let BudgetVarianceOptions {
        months,
        account_type,
        currency,
        coverage,
    } = opts;
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
                .meta(Operation::ReportBudgetVariance, Some(company))
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
    month: Option<ReportMonth>,
    from: Option<ReportMonth>,
    to: Option<ReportMonth>,
) -> Result<db::BudgetMonths, CliError> {
    if let Some(month) = month {
        let month = month
            .resolve()
            .map_err(|number| CliError::Validation(format!("month must be 1-12, got {number}")))?;
        return db::BudgetMonths::new(year, month, month);
    }
    let resolve = |month: Option<ReportMonth>, default| {
        month
            .map_or(Ok(default), ReportMonth::resolve)
            .map_err(|_| CliError::Validation("from/to months must be 1-12".into()))
    };
    db::BudgetMonths::new(
        year,
        resolve(from, Month::January)?,
        resolve(to, Month::December)?,
    )
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
