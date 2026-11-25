mod request;

use crate::operation::Operation;
use std::io::Write;

use beankeeper::types::{AccountCode, AccountType, Currency};
use chrono::{Month, NaiveDate};
use serde::Serialize;

use super::report_periods::MultiReportKind;
use super::{
    report_aging, report_cash_flow, report_compare, report_ledger, report_periods,
    report_reconciliation, report_tax, report_trend,
};
use crate::cli::{Cli, Command, OutputFormat, ReportCommand, resolve_format};
use crate::commands::report_currency::resolve_report_currency;
use crate::db;
use crate::db::{
    companies::{Company, CompanySlug},
    connection::{Db, ReadConn},
};
use crate::error::CliError;
use crate::output;
use crate::output::chart::{self, Chart, WithChart};
use crate::output::json::{Envelope, Meta};
pub(crate) use request::{Comparison, ReportQuery, ReportScope};

pub use super::report_dates::DateRange;
use super::report_dates::DateWindow;
use request::ComparisonWindow;
pub(crate) use request::{ReportWindow, VarianceMonths};

/// Account selector accepting historical ledger account keys from the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountSelector(String);

impl From<String> for AccountSelector {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<AccountCode> for AccountSelector {
    fn from(value: AccountCode) -> Self {
        Self(value.as_ref().to_owned())
    }
}

impl AsRef<str> for AccountSelector {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Optional chart projection accompanying a report's structured data.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) enum ChartRequest {
    #[default]
    None,
    Compact,
    All,
}

impl TryFrom<(bool, bool)> for ChartRequest {
    type Error = CliError;

    fn try_from((chart, all): (bool, bool)) -> Result<Self, Self::Error> {
        match (chart, all) {
            (true, false) => Ok(Self::Compact),
            (true, true) => Ok(Self::All),
            (false, false) => Ok(Self::None),
            (false, true) => Err(CliError::Usage("--chart-all requires --chart".into())),
        }
    }
}

impl ChartRequest {
    pub(crate) fn validate_cli(self, cli: &Cli) -> Result<(), CliError> {
        if matches!(self, Self::None) {
            return Ok(());
        }
        if !cli.is_json() {
            return Err(CliError::Usage(
                "--chart requires --json or --format json".into(),
            ));
        }
        if matches!(cli.command, Command::Summary(_) | Command::Report(_)) {
            return Ok(());
        }
        Err(CliError::Usage(
            "--chart supports summary and financial reports".into(),
        ))
    }

    pub(super) fn project(
        self,
        chart: impl FnOnce(bool) -> Result<Chart, CliError>,
    ) -> Result<Option<Chart>, CliError> {
        match self {
            Self::None => Ok(None),
            Self::Compact => chart(false).map(Some),
            Self::All => chart(true).map(Some),
        }
    }
}

/// Run a `bk report` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] if the subcommand fails.
pub fn run(cli: &Cli, company: &str, sub: &ReportCommand) -> Result<(), CliError> {
    let chart = ChartRequest::try_from((cli.output.chart, cli.output.chart_all))?;
    chart.validate_cli(cli)?;
    let company = company.parse()?;
    let request = ReportRequest::try_from((&company, sub, chart))?;
    let db = cli.command_db()?;
    request.execute(&db)?.render(cli)
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

/// Typed request for a company-scoped financial report.
pub struct ReportRequest {
    company: CompanySlug,
    query: ReportQuery,
    chart: ChartRequest,
}

impl ReportRequest {
    pub(crate) fn new(
        company: CompanySlug,
        query: ReportQuery,
        chart: ChartRequest,
    ) -> Result<Self, CliError> {
        if !matches!(chart, ChartRequest::None) && !query.supports_chart() {
            return Err(CliError::Usage("--chart supports summary, trend, cash-flow, income-statement and period/comparison trial-balance reports".into()));
        }
        Ok(Self {
            company,
            query,
            chart,
        })
    }

    /// The report's canonical operation, including period and comparison modes.
    #[must_use]
    pub fn operation(&self) -> Operation {
        self.query.operation()
    }

    /// Compute a report inside one consistent ledger snapshot.
    ///
    /// # Errors
    /// Returns a named error for an unknown company, missing accounts, ambiguous
    /// currency, unavailable data, unsupported amount ranges, or a database error.
    pub(crate) fn execute(self, db: &Db) -> Result<ReportResponse, CliError> {
        let operation = self.operation();
        db.read_snapshot(move |reader| {
            let scope = Company::bind(reader, self.company.clone())?;
            Ok(ReportResponse {
                operation,
                company: self.company,
                chart: self.chart,
                data: ReportReader {
                    reader,
                    company: &scope,
                }
                .read(self.query)?,
            })
        })
    }
}

/// Owned financial report values, ready to render or serialize.
pub struct ReportResponse {
    operation: Operation,
    company: CompanySlug,
    chart: ChartRequest,
    data: ReportData,
}

impl ReportResponse {
    /// Render the completed report for the CLI with the chosen output format.
    ///
    /// # Errors
    /// Returns a chart, serialization or output error.
    pub fn render(&self, cli: &Cli) -> Result<(), CliError> {
        let mut stdout = std::io::stdout().lock();
        let format = resolve_format(
            match &cli.command {
                Command::Report(args) => match &args.command {
                    ReportCommand::ScheduleC(args) => args.output_format,
                    ReportCommand::Vendor1099(args) => args.period.output_format,
                    _ => None,
                },
                _ => None,
            },
            cli,
        );
        let mut output = ReportOutput::new(
            format,
            output::should_use_color(cli.verbosity.no_color),
            &mut stdout,
        );
        let context =
            ReportPresentation::cli(cli, self.operation, self.company.as_str(), self.chart);
        self.data.render(&context, &mut output)
    }

    /// Serialize the report data for structured MCP content.
    ///
    /// # Errors
    /// Returns a chart or serialization error.
    pub fn data(&self) -> Result<serde_json::Value, CliError> {
        let mut writer = std::io::sink();
        let mut payload = None;
        let mut output = ReportOutput {
            json_payload: Some(&mut payload),
            ..ReportOutput::new(OutputFormat::Json, false, &mut writer)
        };
        let context = ReportPresentation {
            operation: self.operation,
            company: self.company.as_str(),
            chart: self.chart,
            quiet: true,
            cli: None,
        };
        self.data.render(&context, &mut output)?;
        payload.ok_or_else(|| CliError::General("report did not produce a data payload".into()))
    }
}

pub(super) struct ReportPresentation<'a> {
    operation: Operation,
    pub(super) company: &'a str,
    pub(super) chart: ChartRequest,
    pub(super) quiet: bool,
    cli: Option<&'a Cli>,
}

impl<'a> ReportPresentation<'a> {
    pub(super) fn cli(
        cli: &'a Cli,
        operation: Operation,
        company: &'a str,
        chart: ChartRequest,
    ) -> Self {
        Self {
            operation,
            company,
            chart,
            quiet: cli.verbosity.quiet,
            cli: Some(cli),
        }
    }

    pub(super) fn meta(&self) -> Meta {
        let company = Some(self.company);
        self.cli.map_or_else(
            || output::json::meta(self.operation, company),
            |cli| cli.meta(self.operation, company),
        )
    }
}

enum ReportData {
    Comparison(report_compare::ComparisonResponse),
    Periods(report_periods::MultiColumnReport),
    ScheduleC(report_tax::TaxReport<report_tax::ScheduleSummary>),
    Vendor1099(report_tax::TaxReport<report_tax::VendorSummary>),
    Trend(report_trend::TrendReport),
    TrialBalance(StatementWindow, Vec<db::BalanceRow>),
    Balance(StatementWindow, db::BalanceRow),
    IncomeStatement(output::statements::IncomeStatement),
    BalanceSheet(output::statements::BalanceSheet),
    TaxSummary(DateRange, Currency, Vec<db::TaxSummaryRow>),
    BudgetVariance(db::BudgetVarianceReport),
    Reconciliation(report_reconciliation::ReconciliationReport),
    Ledger(report_ledger::LedgerReport),
    Aging(report_aging::AgingResponse),
    CashFlow(report_cash_flow::CashFlowStatement),
}

struct StatementWindow {
    dates: DateWindow,
    currency: Currency,
}

struct ReportReader<'a> {
    reader: ReadConn<'a>,
    company: &'a Company<'a>,
}

impl ReportReader<'_> {
    fn currency(&self, requested: Option<Currency>) -> Result<Currency, CliError> {
        resolve_report_currency(self.reader, self.company.slug().as_str(), requested)
    }

    fn comparison(
        &self,
        kind: report_compare::Kind<'_>,
        scope: ReportScope,
        options: &Comparison,
    ) -> Result<ReportData, CliError> {
        report_compare::compute(self.reader, self.company, kind, scope, options)
            .map(ReportData::Comparison)
    }

    fn compare_or(
        &self,
        kind: report_compare::Kind<'_>,
        scope: ReportScope,
        window: &ComparisonWindow,
        single: impl FnOnce() -> Result<ReportData, CliError>,
    ) -> Result<ReportData, CliError> {
        match &window.0 {
            Some(options) => self.comparison(kind, scope, options),
            None => single(),
        }
    }

    fn periods(
        &self,
        kind: MultiReportKind,
        period: report_periods::PeriodArg,
        scope: ReportScope,
        account_type: Option<AccountType>,
    ) -> Result<ReportData, CliError> {
        report_periods::compute(
            self.company,
            kind,
            period,
            scope.dates.try_into()?,
            self.currency(scope.currency)?,
            account_type,
        )
        .map(ReportData::Periods)
    }

    fn read(self, query: ReportQuery) -> Result<ReportData, CliError> {
        use report_compare::Kind;

        match query {
            ReportQuery::TrialBalance {
                scope,
                account_type,
                window,
            } => {
                self.financial_statement(MultiReportKind::TrialBalance, scope, window, account_type)
            }
            ReportQuery::IncomeStatement { scope, window } => {
                self.financial_statement(MultiReportKind::IncomeStatement, scope, window, None)
            }
            ReportQuery::Balance {
                scope,
                account,
                window,
            } => self.compare_or(Kind::Balance(account.as_ref()), scope, &window, || {
                self.balance(scope, &account)
            }),
            ReportQuery::BalanceSheet { scope, window } => {
                self.compare_or(Kind::BalanceSheet, scope, &window, || {
                    self.balance_sheet(scope)
                })
            }
            ReportQuery::Trend {
                scope,
                period,
                metric,
                window,
            } => self.compare_or(Kind::Trend(period, metric), scope, &window, || {
                self.trend(scope, period, metric)
            }),
            ReportQuery::CashFlow {
                scope,
                accounts,
                window,
            } => self.compare_or(Kind::CashFlow(&accounts), scope, &window, || {
                self.cash_flow(scope, &accounts)
            }),
            ReportQuery::TaxSummary { dates, currency } => self.tax_summary(dates, currency),
            ReportQuery::ScheduleC { dates, currency } => report_tax::build_schedule_c_on(
                self.reader,
                self.company,
                dates,
                self.currency(currency)?,
            )
            .map(ReportData::ScheduleC),
            ReportQuery::Vendor1099 {
                dates,
                currency,
                threshold,
            } => report_tax::build_vendors_on(
                self.reader,
                self.company,
                dates,
                (threshold, self.currency(currency)?).try_into()?,
            )
            .map(ReportData::Vendor1099),
            ReportQuery::BudgetVariance {
                months,
                account_type,
                currency,
                coverage,
            } => self.budget_variance(months, account_type, currency, coverage),
            ReportQuery::Ledger {
                scope,
                accounts,
                filters,
            } => report_ledger::compute(
                self.reader,
                self.company,
                scope.dates,
                &accounts,
                scope.currency,
                filters,
            )
            .map(ReportData::Ledger),
            ReportQuery::Reconciliation(request) => request
                .compute(self.reader, self.company)
                .map(ReportData::Reconciliation),
            ReportQuery::Aging(request) => request
                .compute(self.reader, self.company)
                .map(ReportData::Aging),
        }
    }

    fn financial_statement(
        &self,
        kind: MultiReportKind,
        scope: ReportScope,
        window: ReportWindow,
        account_type: Option<AccountType>,
    ) -> Result<ReportData, CliError> {
        use report_compare::Kind;

        match window {
            ReportWindow::Single => match kind {
                MultiReportKind::TrialBalance => self.trial_balance(scope, account_type),
                MultiReportKind::IncomeStatement => compute_income_statement_on(
                    self.company,
                    scope.dates.try_into()?,
                    self.currency(scope.currency)?,
                )
                .map(ReportData::IncomeStatement),
            },
            ReportWindow::Periods(period) => self.periods(kind, period, scope, account_type),
            ReportWindow::Compare(options) => {
                let comparison = match kind {
                    MultiReportKind::TrialBalance => Kind::TrialBalance(account_type),
                    MultiReportKind::IncomeStatement => Kind::IncomeStatement,
                };
                self.comparison(comparison, scope, &options)
            }
        }
    }

    fn trial_balance(
        &self,
        scope: ReportScope,
        account_type: Option<AccountType>,
    ) -> Result<ReportData, CliError> {
        let currency = self.currency(scope.currency)?;
        let rows = db::compute_trial_balance(
            self.company,
            account_type,
            scope.dates.from(),
            Some(scope.dates.to()),
            currency,
        )?;
        Ok(ReportData::TrialBalance(
            StatementWindow {
                dates: scope.dates,
                currency,
            },
            rows,
        ))
    }

    fn balance(
        &self,
        scope: ReportScope,
        account: &AccountSelector,
    ) -> Result<ReportData, CliError> {
        let account = db::get_account(self.company, account.as_ref())?;
        let currency = self.currency(scope.currency)?;
        let (debit_total, credit_total) = db::compute_account_balance(
            self.company,
            &account.code,
            scope.dates.from(),
            Some(scope.dates.to()),
            currency,
        )?;
        Ok(ReportData::Balance(
            StatementWindow {
                dates: scope.dates,
                currency,
            },
            db::BalanceRow {
                code: account.code,
                name: account.name,
                account_type: account.account_type,
                debit_total,
                credit_total,
            },
        ))
    }

    fn balance_sheet(&self, scope: ReportScope) -> Result<ReportData, CliError> {
        compute_balance_sheet_on(
            self.company,
            scope.dates.to(),
            self.currency(scope.currency)?,
        )
        .map(ReportData::BalanceSheet)
    }

    fn trend(
        &self,
        scope: ReportScope,
        period: report_periods::PeriodArg,
        metric: report_trend::TrendMetric,
    ) -> Result<ReportData, CliError> {
        report_trend::compute_trend_on(
            self.company,
            report_trend::TrendRequest {
                dates: scope.dates.try_into()?,
                currency: self.currency(scope.currency)?,
                metric,
                period,
            },
        )
        .map(ReportData::Trend)
    }

    fn cash_flow(
        &self,
        scope: ReportScope,
        accounts: &[AccountSelector],
    ) -> Result<ReportData, CliError> {
        report_cash_flow::compute_scoped(
            self.reader,
            self.company,
            scope.dates.try_into()?,
            self.currency(scope.currency)?,
            accounts.iter().map(AsRef::as_ref),
        )
        .map(ReportData::CashFlow)
    }

    fn budget_variance(
        &self,
        months: db::BudgetMonths,
        account_type: Option<AccountType>,
        currency: Option<Currency>,
        coverage: db::BudgetCoverage,
    ) -> Result<ReportData, CliError> {
        let params = db::BudgetVarianceParams {
            company_slug: self.company.slug(),
            currency: self.currency(currency)?,
            months,
            account_type,
            coverage,
        };
        db::budgets::compute_budget_variance_on(self.reader, &params)
            .map(ReportData::BudgetVariance)
    }

    fn tax_summary(
        &self,
        dates: DateRange,
        currency: Option<Currency>,
    ) -> Result<ReportData, CliError> {
        let currency = self.currency(currency)?;
        let rows =
            db::compute_tax_summary(self.company, Some(dates.from()), Some(dates.to()), currency)?;
        Ok(ReportData::TaxSummary(dates, currency, rows))
    }
}

impl ReportData {
    fn render(
        &self,
        context: &ReportPresentation<'_>,
        output: &mut ReportOutput<'_>,
    ) -> Result<(), CliError> {
        match self {
            Self::Comparison(report) => report_compare::render(context, report, output),
            Self::Periods(report) => report_periods::render(context, report, output),
            Self::ScheduleC(report) => report_tax::render_schedule(context, report, output),
            Self::Vendor1099(report) => report_tax::render_vendors(context, report, output),
            Self::Trend(report) => report_trend::render(context, report, output),
            Self::TrialBalance(window, rows) => render_trial_balance(context, window, rows, output),
            Self::Balance(window, row) => render_balance(context, row, window, output),
            Self::IncomeStatement(statement) => render_income_statement(context, statement, output),
            Self::BalanceSheet(sheet) => render_balance_sheet(context, sheet, output),
            Self::TaxSummary(dates, currency, rows) => {
                render_tax_summary(context, *dates, *currency, rows, output)
            }
            Self::BudgetVariance(report) => render_budget_variance(context, report, output),
            Self::Ledger(report) => report_ledger::render(context, report, output),
            Self::Aging(report) => report.render(context, output),
            Self::Reconciliation(report) => report_reconciliation::render(context, report, output),
            Self::CashFlow(report) => report_cash_flow::render(context, report, output),
        }
    }
}

fn render_trial_balance(
    context: &ReportPresentation<'_>,
    window: &StatementWindow,
    balances: &[db::BalanceRow],
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let currency = window.currency;
    let (from, to) = (window.dates.from(), Some(window.dates.to()));
    match output.format {
        OutputFormat::Table => {
            let title = build_period_title("Trial Balance", currency.code(), from, to);
            let rendered = output::table::render_trial_balance(
                balances,
                currency,
                output.use_color,
                Some(&title),
            )?;
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Json => {
            let meta = context.meta().with_currency(currency);
            output.write_json(
                meta,
                output::json::trial_balance_data(balances, currency)?,
                None,
            )?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_trial_balance(balances)?;
            write!(output.writer, "{rendered}")?;
        }
    }

    if !context.quiet {
        eprintln!("[ok] trial balance generated");
    }

    Ok(())
}

fn render_balance(
    context: &ReportPresentation<'_>,
    balance_row: &db::BalanceRow,
    window: &StatementWindow,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let currency = window.currency;
    let (from, to) = (window.dates.from(), Some(window.dates.to()));
    match output.format {
        OutputFormat::Table => {
            let params = output::table::AccountBalanceParams {
                code: &balance_row.code,
                name: &balance_row.name,
                account_type: balance_row.account_type,
                debit_total: balance_row.debit_total,
                credit_total: balance_row.credit_total,
                currency,
                use_color: output.use_color,
            };
            let title = build_period_title("Account Balance", currency.code(), from, to);
            let rendered = output::table::render_account_balance(&params);
            print_period_table(&rendered, &title, output.writer)?;
        }
        OutputFormat::Json => {
            let meta = context.meta().with_currency(currency);
            output.write_json(
                meta,
                output::json::account_balance_data(balance_row, currency),
                None,
            )?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_account_balance(balance_row, currency.code())?;
            write!(output.writer, "{rendered}")?;
        }
    }

    if !context.quiet {
        eprintln!(
            "[ok] balance for account {} ({})",
            balance_row.code, balance_row.name
        );
    }

    Ok(())
}

fn render_income_statement(
    context: &ReportPresentation<'_>,
    stmt: &output::statements::IncomeStatement,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    match output.format {
        OutputFormat::Table => {
            let title = build_period_title(
                "Income Statement",
                stmt.currency().code(),
                Some(stmt.dates().from()),
                Some(stmt.dates().to()),
            );
            let rendered =
                output::statements::render_income_statement_table(stmt, &title, output.use_color);
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Json => {
            let meta = context.meta().with_currency(stmt.currency());
            let chart = context
                .chart
                .project(|all| chart::income_statement(context.company, stmt, all))?;
            output.write_json(meta, stmt, chart)?;
        }
        OutputFormat::Csv => {
            let rendered = output::statements::render_income_statement_csv(stmt)?;
            write!(output.writer, "{rendered}")?;
        }
    }
    if !context.quiet {
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
    db.read_snapshot(|reader| {
        let scope = Company::bind(reader, company.parse()?)?;
        compute_income_statement_on(&scope, dates, currency)
    })
}

fn compute_income_statement_on(
    scope: &Company<'_>,
    dates: DateRange,
    currency: Currency,
) -> Result<output::statements::IncomeStatement, CliError> {
    let load = |kind: AccountType| {
        db::compute_trial_balance(
            scope,
            Some(kind),
            Some(dates.from()),
            Some(dates.to()),
            currency,
        )
    };
    output::statements::build_income_statement(
        dates,
        currency,
        load(AccountType::Revenue)?,
        load(AccountType::Expense)?,
    )
}

fn render_balance_sheet(
    context: &ReportPresentation<'_>,
    sheet: &output::statements::BalanceSheet,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    match output.format {
        OutputFormat::Table => {
            let title = build_period_title(
                "Balance Sheet",
                sheet.currency().code(),
                None,
                Some(sheet.as_of()),
            );
            let rendered =
                output::statements::render_balance_sheet_table(sheet, &title, output.use_color);
            writeln!(output.writer, "{rendered}")?;
            for balance in &sheet.fx_balances {
                writeln!(output.writer, "{balance}")?;
            }
        }
        OutputFormat::Json => {
            let meta = context.meta().with_currency(sheet.currency());
            output.write_json(meta, sheet, None)?;
        }
        OutputFormat::Csv => {
            let rendered = output::statements::render_balance_sheet_csv(sheet)?;
            write!(output.writer, "{rendered}")?;
        }
    }
    if !context.quiet {
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
    db.read_snapshot(|reader| {
        let scope = Company::bind(reader, company.parse()?)?;
        compute_balance_sheet_on(&scope, as_of, currency)
    })
}

fn compute_balance_sheet_on(
    scope: &Company<'_>,
    as_of: NaiveDate,
    currency: Currency,
) -> Result<output::statements::BalanceSheet, CliError> {
    let load = |kind: AccountType| {
        db::compute_trial_balance(scope, Some(kind), None, Some(as_of), currency)
    };
    let mut sheet =
        output::statements::build_balance_sheet(output::statements::BalanceSheetInputs {
            as_of,
            currency,
            assets: load(AccountType::Asset)?,
            liabilities: load(AccountType::Liability)?,
            equity: load(AccountType::Equity)?,
            revenue: load(AccountType::Revenue)?,
            expenses: load(AccountType::Expense)?,
        })?;
    sheet.fx_balances = db::fx::balances(scope, Some(as_of))?;
    Ok(sheet)
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
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> String {
    match (from, to) {
        (Some(f), Some(t)) => format!("{base} ({f} to {t}) ({currency_code})"),
        (Some(f), None) => format!("{base} (from {f}) ({currency_code})"),
        (None, Some(t)) => format!("{base} (through {t}) ({currency_code})"),
        (None, None) => format!("{base} ({currency_code})"),
    }
}

fn render_tax_summary(
    context: &ReportPresentation<'_>,
    dates: DateRange,
    currency: Currency,
    rows: &[db::TaxSummaryRow],
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let (from, to) = (Some(dates.from()), Some(dates.to()));
    match output.format {
        OutputFormat::Table => {
            let title = build_period_title("Tax Summary", currency.code(), from, to);
            let rendered = output::table::render_tax_summary(rows, currency, output.use_color);
            print_period_table(&rendered, &title, output.writer)?;
        }
        OutputFormat::Json => {
            let meta = context.meta().with_currency(currency);
            output.write_json(meta, output::json::tax_summary_data(rows, currency), None)?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_tax_summary(rows)?;
            write!(output.writer, "{rendered}")?;
        }
    }

    if !context.quiet {
        eprintln!("[ok] tax summary generated ({} categories)", rows.len());
    }

    Ok(())
}

fn render_budget_variance(
    context: &ReportPresentation<'_>,
    report: &db::BudgetVarianceReport,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    match output.format {
        OutputFormat::Table => {
            let title = build_variance_title(report);
            let rendered = output::table::render_budget_variance(report, &title, output.use_color);
            writeln!(output.writer, "{rendered}")?;
        }
        OutputFormat::Json => {
            let meta = context.meta().with_currency(report.currency());
            output.write_json(meta, report, None)?;
        }
        OutputFormat::Csv => {
            let rendered = output::csv::render_budget_variance(report)?;
            write!(output.writer, "{rendered}")?;
        }
    }

    if !context.quiet {
        eprintln!(
            "[ok] budget variance generated ({} accounts)",
            report.rows().len()
        );
    }
    Ok(())
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
