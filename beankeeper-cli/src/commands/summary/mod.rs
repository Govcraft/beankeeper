//! Advisor-ready, read-only financial snapshot shared by CLI and MCP.

mod attention;
mod model;
mod owner;
mod window;

pub use owner::{OwnerSummaryRead, OwnerSummaryReader};
pub use window::SummaryWindow;

use super::report::{AccountSelector, ChartRequest};
use super::report_dates::DateRange;
use crate::operation::Operation;
use crate::request::LocalDate;
use std::io::Write;

use beankeeper::types::{AccountCode, Currency, Money};
use chrono::{Datelike, Days, NaiveDate};
use clap::Args;
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::{Cli, OutputFormat, resolve_format};
use crate::db::{
    self, Actor, BalanceRow,
    companies::{Company, CompanySlug},
    connection::{Db, ReadConn},
    holds::StaleHoldAge,
};
use crate::error::CliError;
use crate::output::amounts::MoneyField;
use crate::output::chart::{self, Chart, WithChart};
use crate::output::json::Envelope;

/// Options for `bk summary`.
#[derive(Debug, Clone, Default, Args)]
pub struct SummaryArgs {
    /// First date, inclusive (YYYY-MM-DD). Defaults to the end date's month start.
    #[arg(long)]
    pub from: Option<crate::cli::CalendarDateArg>,
    /// Last date, inclusive (YYYY-MM-DD). Defaults to today.
    #[arg(long)]
    pub to: Option<crate::cli::CalendarDateArg>,
    /// First comparison date. Requires --compare-to.
    #[arg(long)]
    pub compare_from: Option<crate::cli::CalendarDateArg>,
    /// Last comparison date. Requires --compare-from.
    #[arg(long)]
    pub compare_to: Option<crate::cli::CalendarDateArg>,
    /// Currency code; required when the company's currencies are ambiguous.
    #[arg(long)]
    pub currency: Option<Currency>,
    /// Cash asset account (repeatable). Defaults to cash/bank/checking/savings names.
    #[arg(long = "cash-account")]
    pub cash_account: Vec<String>,
    /// Receivable asset account (repeatable). Defaults to names containing receivable.
    #[arg(long = "receivable-account")]
    pub receivable_account: Vec<String>,
    /// Show open card holds older than this many days.
    #[arg(long, default_value_t)]
    pub stale_hold_age_days: StaleHoldAge,
    /// Payable liability account (repeatable). Defaults to names containing payable.
    #[arg(long = "payable-account")]
    pub payable_account: Vec<String>,
}

#[derive(Clone, Copy, Serialize)]
struct Period {
    from: NaiveDate,
    to: NaiveDate,
}

impl From<DateRange> for Period {
    fn from(range: DateRange) -> Self {
        Self {
            from: range.from(),
            to: range.to(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct Periods {
    current: Period,
    prior: Period,
}

impl Periods {
    pub(crate) fn windows(&self) -> [(NaiveDate, NaiveDate); 2] {
        [
            (self.current.from, self.current.to),
            (self.prior.from, self.prior.to),
        ]
    }
}

pub(crate) use model::Comparison;

/// Date selection with an ordered, optional pair of comparison dates.
#[derive(Clone, Copy, Default)]
pub struct SummaryDates {
    current: SummaryWindow,
    comparison: Option<DateRange>,
}

/// Error validating the supplied summary dates.
#[derive(Debug)]
pub enum SummaryDatesError {
    /// Only one endpoint of the comparison was supplied.
    IncompleteComparison,
    /// The supplied range starts after it ends.
    InvalidRange(CliError),
}

impl SummaryDatesError {
    const INCOMPLETE_COMPARISON: &str = "provide both comparison dates";
}

impl std::fmt::Display for SummaryDatesError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompleteComparison => formatter.write_str(Self::INCOMPLETE_COMPARISON),
            Self::InvalidRange(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SummaryDatesError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::IncompleteComparison => None,
            Self::InvalidRange(error) => Some(error),
        }
    }
}

impl From<SummaryDatesError> for CliError {
    fn from(error: SummaryDatesError) -> Self {
        match error {
            SummaryDatesError::IncompleteComparison => {
                Self::Validation(SummaryDatesError::INCOMPLETE_COMPARISON.into())
            }
            SummaryDatesError::InvalidRange(error) => error,
        }
    }
}

impl TryFrom<[Option<NaiveDate>; 4]> for SummaryDates {
    type Error = SummaryDatesError;

    fn try_from(
        [from, to, compare_from, compare_to]: [Option<NaiveDate>; 4],
    ) -> Result<Self, Self::Error> {
        let comparison = match (compare_from, compare_to) {
            (Some(from), Some(to)) => {
                Some(DateRange::new(from, to).map_err(SummaryDatesError::InvalidRange)?)
            }
            (None, None) => None,
            _ => return Err(SummaryDatesError::IncompleteComparison),
        };
        let current =
            SummaryWindow::try_from((from, to)).map_err(SummaryDatesError::InvalidRange)?;
        Ok((current, comparison).into())
    }
}

impl From<(SummaryWindow, Option<DateRange>)> for SummaryDates {
    fn from((current, comparison): (SummaryWindow, Option<DateRange>)) -> Self {
        Self {
            current,
            comparison,
        }
    }
}

impl SummaryDates {
    fn periods(self, today: LocalDate) -> Result<Periods, CliError> {
        let current = self.current.period(today.into())?;
        let prior = match self.comparison {
            Some(range) => range.into(),
            None if self.current.start().is_none() => {
                let to = current.from.pred_opt().ok_or_else(date_error)?;
                Period {
                    from: to.with_day(1).ok_or_else(date_error)?,
                    to,
                }
            }
            None => {
                let days = u64::try_from((current.to - current.from).num_days() + 1)
                    .map_err(|_| date_error())?;
                let to = current.from.pred_opt().ok_or_else(date_error)?;
                Period {
                    from: current
                        .from
                        .checked_sub_days(Days::new(days))
                        .ok_or_else(date_error)?,
                    to,
                }
            }
        };
        Ok(Periods { current, prior })
    }
}

impl TryFrom<SummaryDates> for Periods {
    type Error = CliError;

    fn try_from(dates: SummaryDates) -> Result<Self, Self::Error> {
        dates.periods(LocalDate::from(None))
    }
}

impl TryFrom<&SummaryArgs> for SummaryDates {
    type Error = CliError;

    fn try_from(args: &SummaryArgs) -> Result<Self, Self::Error> {
        Self::try_from([
            args.from.map(Into::into),
            args.to.map(Into::into),
            args.compare_from.map(Into::into),
            args.compare_to.map(Into::into),
        ])
        .map_err(|error| match error {
            SummaryDatesError::IncompleteComparison => {
                CliError::Usage(SummaryDatesError::INCOMPLETE_COMPARISON.into())
            }
            SummaryDatesError::InvalidRange(error) => error,
        })
    }
}

/// Company-scoped inputs to the financial summary computation.
pub struct SummaryRequest {
    company: CompanySlug,
    periods: Periods,
    currency: Option<Currency>,
    accounts: SummaryAccounts,
    chart: ChartRequest,
    stale_hold_age: StaleHoldAge,
}

/// Optional exact account selections, grouped by their role in the summary.
#[derive(Default)]
pub struct SummaryAccounts {
    pub(crate) cash: Vec<AccountSelector>,
    pub(crate) receivables: Vec<AccountSelector>,
    pub(crate) payables: Vec<AccountSelector>,
}

impl From<[Vec<AccountCode>; 3]> for SummaryAccounts {
    /// Accepts cash, receivable and payable account codes, in that order.
    fn from([cash, receivables, payables]: [Vec<AccountCode>; 3]) -> Self {
        let selectors =
            |codes: Vec<AccountCode>| codes.into_iter().map(AccountSelector::from).collect();
        Self {
            cash: selectors(cash),
            receivables: selectors(receivables),
            payables: selectors(payables),
        }
    }
}

impl SummaryRequest {
    pub(crate) fn new(
        company: CompanySlug,
        dates: SummaryDates,
        currency: Option<Currency>,
        accounts: SummaryAccounts,
        chart: ChartRequest,
        stale_hold_age: StaleHoldAge,
    ) -> Result<Self, CliError> {
        Ok(Self {
            company,
            periods: dates.try_into()?,
            currency,
            accounts,
            chart,
            stale_hold_age,
        })
    }
}

impl TryFrom<(CompanySlug, SummaryDates, Option<Currency>, SummaryAccounts)> for SummaryRequest {
    type Error = CliError;

    fn try_from(
        (company, dates, currency, accounts): (
            CompanySlug,
            SummaryDates,
            Option<Currency>,
            SummaryAccounts,
        ),
    ) -> Result<Self, Self::Error> {
        Self::new(
            company,
            dates,
            currency,
            accounts,
            ChartRequest::default(),
            StaleHoldAge::default(),
        )
    }
}

impl TryFrom<(&CompanySlug, &SummaryArgs, (bool, bool))> for SummaryRequest {
    type Error = CliError;

    fn try_from(
        (company, args, chart): (&CompanySlug, &SummaryArgs, (bool, bool)),
    ) -> Result<Self, Self::Error> {
        Self::try_from((company, args, ChartRequest::try_from(chart)?))
    }
}

impl TryFrom<(&CompanySlug, &SummaryArgs, ChartRequest)> for SummaryRequest {
    type Error = CliError;

    fn try_from(
        (company, args, chart): (&CompanySlug, &SummaryArgs, ChartRequest),
    ) -> Result<Self, Self::Error> {
        let accounts = |items: &[String]| {
            items
                .iter()
                .cloned()
                .map(AccountSelector::from)
                .collect::<Vec<_>>()
        };
        Self::new(
            company.clone(),
            SummaryDates::try_from(args)?,
            args.currency,
            SummaryAccounts {
                cash: accounts(&args.cash_account),
                receivables: accounts(&args.receivable_account),
                payables: accounts(&args.payable_account),
            },
            chart,
            args.stale_hold_age_days,
        )
    }
}

fn money(minor_units: i64, currency: Currency) -> Money {
    Money::from_minor(i128::from(minor_units), currency)
}

fn date_error() -> CliError {
    CliError::Validation("summary dates are outside the supported range".into())
}

fn overflow() -> CliError {
    CliError::Validation("summary amount exceeds the supported minor-unit range".into())
}

fn add(left: i64, right: i64) -> Result<i64, CliError> {
    left.checked_add(right).ok_or_else(overflow)
}

fn sub(left: i64, right: i64) -> Result<i64, CliError> {
    left.checked_sub(right).ok_or_else(overflow)
}

fn net(row: &BalanceRow) -> Result<i64, CliError> {
    if row.account_type.is_debit_normal() {
        sub(row.debit_total, row.credit_total)
    } else {
        sub(row.credit_total, row.debit_total)
    }
}

fn sum<'a>(mut rows: impl Iterator<Item = &'a BalanceRow>) -> Result<i64, CliError> {
    rows.try_fold(0, |total, row| add(total, net(row)?))
}

fn select_accounts(
    rows: &[BalanceRow],
    explicit: &[AccountSelector],
    kind: beankeeper::types::AccountType,
    keywords: &[&str],
) -> Result<Vec<String>, CliError> {
    if explicit.is_empty() {
        return Ok(rows
            .iter()
            .filter(|row| {
                row.account_type == kind
                    && keywords
                        .iter()
                        .any(|keyword| row.name.to_lowercase().contains(keyword))
            })
            .map(|row| row.code.clone())
            .collect());
    }
    let mut codes = std::collections::BTreeSet::new();
    for code in explicit.iter().map(AsRef::as_ref) {
        let row = rows
            .iter()
            .find(|row| row.code == code)
            .ok_or_else(|| CliError::NotFound(format!("account '{code}' not found")))?;
        if row.account_type != kind {
            return Err(CliError::Validation(format!(
                "account '{code}' must be a {kind} account"
            )));
        }
        codes.insert(code.to_owned());
    }
    Ok(codes.into_iter().collect())
}

impl SummaryRequest {
    /// Compute a consistent summary without changing any ledger records.
    ///
    /// # Errors
    /// Returns named CLI errors for missing companies/accounts, ambiguous
    /// currencies, amount overflow, or database failures.
    pub(crate) fn execute(self, db: &Db) -> Result<SummaryResponse, CliError> {
        db.read_snapshot(move |reader| {
            let company = self.company.clone();
            let chart_request = self.chart;
            let report = compute(reader, self)?;
            let chart = chart_request.project(|all| {
                chart::summary(
                    company.as_str(),
                    report.currency,
                    &report.periods,
                    &report.comparison,
                    all,
                )
            })?;
            Ok(SummaryResponse {
                company,
                report,
                chart,
            })
        })
    }
}

/// Financial standing, comparison and attention items computed in one snapshot.
pub struct SummaryResponse {
    company: CompanySlug,
    report: SummaryReport,
    chart: Option<Chart>,
}

impl SummaryResponse {
    /// The inclusive `(from, to)` windows, with the current period first and
    /// the comparison period second.
    #[must_use]
    pub fn period_windows(&self) -> [(NaiveDate, NaiveDate); 2] {
        self.report.periods.windows()
    }

    /// The summary's plain-language highlights in report order.
    #[must_use]
    pub fn highlights(&self) -> &[String] {
        &self.report.highlights
    }

    /// The shared summary envelope, including the verified caller if available.
    pub fn envelope(&self, actor: &Actor) -> impl Serialize + '_ {
        let mut meta = crate::output::json::meta(Operation::Summary, Some(self.company.as_str()))
            .with_currency(self.report.currency);
        meta.agent = actor.agent_uri().map(str::to_owned);
        Envelope {
            ok: true,
            meta,
            data: Some(WithChart {
                report: self.report.json(),
                chart: self.chart.as_ref(),
            }),
            error: None,
        }
    }

    /// Render this summary as plain words, JSON, or CSV.
    ///
    /// # Errors
    /// Returns output or serialization errors.
    pub fn render(&self, cli: &Cli) -> Result<(), CliError> {
        let rendered = match resolve_format(None, cli) {
            OutputFormat::Json => serde_json::to_string_pretty(&self.envelope(&cli.principal()))
                .map_err(|error| CliError::General(error.to_string()))?,
            OutputFormat::Table => model::plain(&self.report),
            OutputFormat::Csv => model::csv(&self.report)?,
        };
        writeln!(std::io::stdout().lock(), "{rendered}")?;
        Ok(())
    }
}

struct SummaryReport {
    currency: Currency,
    periods: Periods,
    standing: Standing,
    comparison: Vec<Comparison>,
    movers: Vec<Comparison>,
    attention: attention::Attention,
    highlights: Vec<String>,
    view: Value,
    fx_balances: Vec<db::fx::Balance>,
}

#[derive(Serialize)]
struct Standing {
    as_of: NaiveDate,
    #[serde(flatten)]
    cash_on_hand: MoneyField,
    #[serde(flatten)]
    owed_to_you: MoneyField,
    #[serde(flatten)]
    you_owe: MoneyField,
    #[serde(flatten)]
    net_worth: MoneyField,
    cash_accounts: Vec<String>,
    receivable_accounts: Vec<String>,
    payable_accounts: Vec<String>,
    cash_accounts_identified: bool,
    receivable_accounts_identified: bool,
}

#[derive(Serialize)]
struct StandingSection<'a> {
    #[serde(flatten)]
    standing: &'a Standing,
    desktop_view: &'a Value,
}

#[derive(Serialize)]
struct SeriesSection<'a> {
    series: &'a [Comparison],
    desktop_view: &'a Value,
}

#[derive(Serialize)]
struct AttentionSection<'a> {
    items: &'a attention::Attention,
    desktop_view: &'a Value,
}

#[derive(Serialize)]
struct SummaryJson<'a> {
    currency: &'a str,
    minor_units: u8,
    periods: &'a Periods,
    standing: StandingSection<'a>,
    comparison: SeriesSection<'a>,
    biggest_movers: SeriesSection<'a>,
    needs_attention: AttentionSection<'a>,
    highlights: &'a [String],
    desktop_view: &'a Value,
    basis: Value,
    #[serde(skip_serializing_if = "<[db::fx::Balance]>::is_empty")]
    fx_balances: &'a [db::fx::Balance],
}

impl SummaryReport {
    fn json(&self) -> impl Serialize + '_ {
        SummaryJson {
            currency: self.currency.code(),
            minor_units: self.currency.minor_units(),
            periods: &self.periods,
            standing: StandingSection {
                standing: &self.standing,
                desktop_view: &self.view,
            },
            comparison: SeriesSection {
                series: &self.comparison,
                desktop_view: &self.view,
            },
            biggest_movers: SeriesSection {
                series: &self.movers,
                desktop_view: &self.view,
            },
            needs_attention: AttentionSection {
                items: &self.attention,
                desktop_view: &self.view,
            },
            highlights: &self.highlights,
            fx_balances: &self.fx_balances,
            desktop_view: &self.view,
            basis: json!({
                "activity":"Posted revenue and net expenses, including refunds; leftover is income minus spending, not cash flow.",
                "valuation":"Net worth uses ledger book values, not investment market prices.",
                "accounts":"Cash: asset names containing cash/bank/checking/savings; receivables: asset names containing receivable; payables: liability names containing payable. Explicit selectors replace inferred lists; an empty selection totals zero. You owe includes every liability.",
                "comparison":"Without --from: month to date versus the full prior calendar month. With --from: the immediately preceding equal-length interval. Explicit comparison dates override either default."
            }),
        }
    }
}

fn compute(reader: ReadConn<'_>, request: SummaryRequest) -> Result<SummaryReport, CliError> {
    let SummaryRequest {
        company,
        periods,
        currency,
        accounts,
        stale_hold_age,
        ..
    } = request;
    let scope = Company::bind(reader, company.clone())?;
    let company = company.as_str();
    let currency = super::report_currency::resolve_report_currency(reader, company, currency)?;
    let rows = db::compute_trial_balance(&scope, None, None, Some(periods.current.to), currency)?;
    let cash = select_accounts(
        &rows,
        &accounts.cash,
        beankeeper::types::AccountType::Asset,
        super::report_cash_flow::DEFAULT_CASH_NAME_KEYWORDS,
    )?;
    let receivables = select_accounts(
        &rows,
        &accounts.receivables,
        beankeeper::types::AccountType::Asset,
        &["receivable"],
    )?;
    let payables = select_accounts(
        &rows,
        &accounts.payables,
        beankeeper::types::AccountType::Liability,
        &["payable"],
    )?;
    let activity = |period: Period| {
        db::compute_trial_balance(&scope, None, Some(period.from), Some(period.to), currency)
    };
    let current = activity(periods.current)?;
    let prior = activity(periods.prior)?;
    let comparison = model::comparison(&current, &prior, currency)?;
    let movers = model::movers(&current, &prior, currency)?;
    let attention = attention::compute(&attention::AttentionContext {
        conn: reader,
        company: scope.slug(),
        currency,
        as_of: periods.current.to,
        stale_hold_age,
        cash: &cash,
        receivables: &receivables,
        payables: &payables,
    })?;
    let view = model::desktop_view(
        company,
        currency,
        &periods,
        stale_hold_age,
        &model::SelectedAccounts {
            cash: &cash,
            receivable: &receivables,
            payable: &payables,
        },
    );
    let cash_on_hand = sum(rows.iter().filter(|row| cash.contains(&row.code)))?;
    let owed_to_you = sum(rows.iter().filter(|row| receivables.contains(&row.code)))?;
    let you_owe = sum(rows
        .iter()
        .filter(|row| row.account_type == beankeeper::types::AccountType::Liability))?;
    let assets = sum(rows
        .iter()
        .filter(|row| row.account_type == beankeeper::types::AccountType::Asset))?;
    let field = |name, value| MoneyField::new(name, money(value, currency));
    let standing = Standing {
        as_of: periods.current.to,
        cash_on_hand: field("cash_on_hand", cash_on_hand),
        owed_to_you: field("owed_to_you", owed_to_you),
        you_owe: field("you_owe", you_owe),
        net_worth: field("net_worth", sub(assets, you_owe)?),
        cash_accounts_identified: !cash.is_empty(),
        receivable_accounts_identified: !receivables.is_empty(),
        cash_accounts: cash,
        receivable_accounts: receivables,
        payable_accounts: payables,
    };
    let mut highlights = model::highlights(&standing, &comparison, &movers);
    let fx_balances = db::fx::balances(&scope, Some(periods.current.to))?;
    highlights.extend(fx_balances.iter().map(ToString::to_string));
    Ok(SummaryReport {
        currency,
        periods,
        standing,
        comparison,
        movers,
        attention,
        highlights,
        view,
        fx_balances,
    })
}

/// Render `bk summary` in plain words, JSON, or CSV.
///
/// # Errors
/// Returns summary validation, database, serialization or output errors.
pub fn run(cli: &Cli, company: &str, args: &SummaryArgs) -> Result<(), CliError> {
    let chart = ChartRequest::try_from((cli.output.chart, cli.output.chart_all))?;
    chart.validate_cli(cli)?;
    let company = company.parse()?;
    let request = SummaryRequest::try_from((&company, args, chart))?;
    let db = cli.command_db()?;
    crate::request::LedgerRequest::Summary(request)
        .execute(&db, &cli.principal())?
        .render(cli, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Admin, Granted};

    #[test]
    fn periods_handle_month_edges_leap_years_and_explicit_intervals() -> Result<(), CliError> {
        let today = NaiveDate::from_ymd_opt(2024, 3, 15).ok_or_else(date_error)?;
        let p = SummaryDates::default().periods(today.into())?;
        assert_eq!(p.current.from.to_string(), "2024-03-01");
        assert_eq!(p.prior.from.to_string(), "2024-02-01");
        assert_eq!(p.prior.to.to_string(), "2024-02-29");
        let args = SummaryArgs {
            from: Some("2024-03-10".parse()?),
            to: Some("2024-03-15".parse()?),
            ..SummaryArgs::default()
        };
        let p = SummaryDates::try_from(&args)?.periods(today.into())?;
        assert_eq!(p.prior.from.to_string(), "2024-03-04");
        assert_eq!(p.prior.to.to_string(), "2024-03-09");
        let p = SummaryDates::try_from(&SummaryArgs {
            to: Some("2025-01-01".parse()?),
            ..SummaryArgs::default()
        })?
        .periods(today.into())?;
        assert_eq!(p.prior.from.to_string(), "2024-12-01");
        assert_eq!(p.prior.to.to_string(), "2024-12-31");
        Ok(())
    }

    #[test]
    fn response_exposes_typed_period_windows_and_plain_highlights()
    -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory()?;
        let actor = Actor::new("summary fixture".parse()?);
        let company = "co".parse()?;
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            db::create_company(grant, &company, &"Company".parse()?, None)
        })?;
        let args = SummaryArgs {
            to: Some("2024-03-15".parse()?),
            currency: Some(Currency::USD),
            ..SummaryArgs::default()
        };
        let response = SummaryRequest::try_from((&company, &args, (false, false)))?.execute(&db)?;

        let expected_windows: [(NaiveDate, NaiveDate); 2] = [
            ("2024-03-01".parse()?, "2024-03-15".parse()?),
            ("2024-02-01".parse()?, "2024-02-29".parse()?),
        ];
        assert_eq!(response.period_windows(), expected_windows);
        let highlights = response.highlights();
        assert!(
            highlights
                .first()
                .is_some_and(|line| line.starts_with("No cash accounts were identified"))
        );
        let envelope = serde_json::to_value(response.envelope(&actor))?;
        assert_eq!(json!(highlights), envelope["data"]["highlights"]);
        Ok(())
    }

    #[test]
    fn summary_projects_known_and_unknown_money_without_the_envelope()
    -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory()?;
        Granted::<Admin>::with_db(&db, &db::Actor::new("summary fixture".parse()?), |grant| {
            db::create_company(grant, &"co".parse()?, &"Company".parse()?, None)
        })?;
        let args = SummaryArgs {
            to: Some("2024-03-15".parse()?),
            currency: Some(Currency::BHD),
            ..SummaryArgs::default()
        };
        let report = compute(
            db.read(),
            SummaryRequest::try_from((&"co".parse()?, &args, (false, false)))?,
        )?;
        let json = serde_json::to_value(report.json())?;
        assert_eq!(json["standing"]["cash_on_hand"], 0);
        assert_eq!(json["standing"]["cash_on_hand_display"], "0.000");
        let attention = &json["needs_attention"]["items"];
        assert_eq!(attention["recurring_due"]["total_display"], "0.000");
        assert_eq!(attention["recurring_due"]["from"], "2024-03-16");
        assert_eq!(
            attention["overdue_payables"].get("total"),
            Some(&Value::Null)
        );
        assert_eq!(
            attention["overdue_payables"].get("total_display"),
            Some(&Value::Null)
        );
        assert!(attention["overdue_payables"].get("id").is_none());
        Ok(())
    }

    #[test]
    fn checked_money_rejects_overflow() {
        assert!(add(i64::MAX, 1).is_err());
        assert!(sub(i64::MIN, 1).is_err());
    }
}
