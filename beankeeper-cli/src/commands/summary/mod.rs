//! Advisor-ready, read-only financial snapshot shared by CLI and MCP.

mod attention;
mod model;

use super::report_dates::{DateRange, DateWindow, resolve_date};
use crate::operation::Operation;
use std::io::Write;

use beankeeper::types::{Currency, Money};
use chrono::{Datelike, Days, NaiveDate};
use clap::Args;
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::{Cli, OutputFormat, resolve_format};
use crate::db::{
    self, BalanceRow,
    companies::Company,
    connection::{Db, ReadConn},
};
use crate::error::CliError;
use crate::output::amounts::MoneyField;
use crate::output::chart::{self, WithChart};
use crate::output::json::Envelope;

/// Options for `bk summary` and the `financial_summary` MCP tool.
#[derive(Debug, Clone, Default, Args, serde::Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SummaryArgs {
    /// First date, inclusive (YYYY-MM-DD). Defaults to the end date's month start.
    #[arg(long)]
    pub from: Option<crate::cli::CalendarDateArg>,
    /// Last date, inclusive (YYYY-MM-DD). Defaults to today.
    #[arg(long)]
    pub to: Option<crate::cli::CalendarDateArg>,
    /// First comparison date. Requires --compare-to.
    #[arg(long, requires = "compare_to")]
    pub compare_from: Option<crate::cli::CalendarDateArg>,
    /// Last comparison date. Requires --compare-from.
    #[arg(long, requires = "compare_from")]
    pub compare_to: Option<crate::cli::CalendarDateArg>,
    /// Currency code; required when the company's currencies are ambiguous.
    #[arg(long)]
    #[schemars(with = "Option<String>")]
    pub currency: Option<crate::cli::CurrencyArg>,
    /// Cash asset account (repeatable). Defaults to cash/bank/checking/savings names.
    #[arg(long = "cash-account")]
    #[serde(default)]
    pub cash_account: Vec<String>,
    /// Receivable asset account (repeatable). Defaults to names containing receivable.
    #[arg(long = "receivable-account")]
    #[serde(default)]
    pub receivable_account: Vec<String>,
    /// Payable liability account (repeatable). Defaults to names containing payable.
    #[arg(long = "payable-account")]
    #[serde(default)]
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

impl SummaryArgs {
    fn periods(&self, today: NaiveDate) -> Result<Periods, CliError> {
        let end = DateWindow::resolve(None, self.to.as_ref(), today)?.to;
        let month_start = end.with_day(1).ok_or_else(date_error)?;
        let start = self
            .from
            .as_ref()
            .map_or(Ok(month_start), |from| resolve_date("--from", from))?;
        let current = Period::from(DateRange::new(start, end)?);
        let prior = match (&self.compare_from, &self.compare_to) {
            (Some(from), Some(to)) => DateRange::resolve(Some(from), Some(to), today)?.into(),
            (None, None) if self.from.is_none() => {
                let to = month_start.pred_opt().ok_or_else(date_error)?;
                Period {
                    from: to.with_day(1).ok_or_else(date_error)?,
                    to,
                }
            }
            (None, None) => {
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
            _ => return Err(CliError::Validation("provide both comparison dates".into())),
        };
        Ok(Periods { current, prior })
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
    explicit: &[String],
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
    for code in explicit {
        let row = rows
            .iter()
            .find(|row| &row.code == code)
            .ok_or_else(|| CliError::NotFound(format!("account '{code}' not found")))?;
        if row.account_type != kind {
            return Err(CliError::Validation(format!(
                "account '{code}' must be a {kind} account"
            )));
        }
        codes.insert(code.clone());
    }
    Ok(codes.into_iter().collect())
}

/// Compute a consistent summary envelope without changing any ledger records.
///
/// Dates use local today. All reads share one SQLite snapshot, also when called
/// inside an MCP request's authorization snapshot.
///
/// # Errors
/// Returns named CLI errors for invalid options, missing companies/accounts,
/// ambiguous currencies, amount overflow, or database failures.
pub fn execute_json(
    cli: &Cli,
    db: &Db,
    company: &str,
    args: &SummaryArgs,
) -> Result<Value, CliError> {
    let report = read_summary(cli, db, company, args)?;
    envelope(cli, company, &report)
}

fn read_summary(
    cli: &Cli,
    db: &Db,
    company: &str,
    args: &SummaryArgs,
) -> Result<SummaryReport, CliError> {
    chart::validate_request(cli)?;
    db.read_snapshot(|reader| compute(reader, company, args))
}

fn envelope(cli: &Cli, company: &str, report: &SummaryReport) -> Result<Value, CliError> {
    let meta = cli
        .meta(Operation::Summary, Some(company))
        .with_currency(report.currency);
    let chart = chart::requested(cli, || {
        chart::summary(
            company,
            report.currency,
            &report.periods,
            &report.comparison,
            cli.output.chart_all,
        )
    })?;
    serde_json::to_value(Envelope {
        ok: true,
        meta,
        data: Some(WithChart {
            report: report.json(),
            chart,
        }),
        error: None,
    })
    .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
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

fn compute(
    reader: ReadConn<'_>,
    company: &str,
    args: &SummaryArgs,
) -> Result<SummaryReport, CliError> {
    let scope = Company::bind(reader, company.parse()?)?;
    let periods = args.periods(chrono::Local::now().date_naive())?;
    let currency = super::report_currency::resolve_report_currency(
        reader,
        company,
        crate::cli::CurrencyArg::resolve_optional(args.currency.as_ref())?,
    )?;
    let rows = db::compute_trial_balance(&scope, None, None, Some(periods.current.to), currency)?;
    let cash = select_accounts(
        &rows,
        &args.cash_account,
        beankeeper::types::AccountType::Asset,
        super::report_cash_flow::DEFAULT_CASH_NAME_KEYWORDS,
    )?;
    let receivables = select_accounts(
        &rows,
        &args.receivable_account,
        beankeeper::types::AccountType::Asset,
        &["receivable"],
    )?;
    let payables = select_accounts(
        &rows,
        &args.payable_account,
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
        company,
        currency,
        as_of: periods.current.to,
        cash: &cash,
        receivables: &receivables,
        payables: &payables,
    })?;
    let view = model::desktop_view(
        company,
        currency,
        &periods,
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
    let highlights = model::highlights(&standing, &comparison, &movers);
    Ok(SummaryReport {
        currency,
        periods,
        standing,
        comparison,
        movers,
        attention,
        highlights,
        view,
    })
}

/// Render `bk summary` in plain words, JSON, or CSV.
///
/// # Errors
/// Returns summary validation, database, serialization or output errors.
pub fn run(cli: &Cli, company: &str, args: &SummaryArgs) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let report = read_summary(cli, &db, company, args)?;
    let rendered = match resolve_format(None, cli) {
        OutputFormat::Json => serde_json::to_string_pretty(&envelope(cli, company, &report)?)
            .map_err(|error| CliError::General(error.to_string()))?,
        OutputFormat::Table => model::plain(&report),
        OutputFormat::Csv => model::csv(&report)?,
    };
    writeln!(std::io::stdout().lock(), "{rendered}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Admin, Granted};

    #[test]
    fn periods_handle_month_edges_leap_years_and_explicit_intervals() -> Result<(), CliError> {
        let today = NaiveDate::from_ymd_opt(2024, 3, 15).ok_or_else(date_error)?;
        let p = SummaryArgs::default().periods(today)?;
        assert_eq!(p.current.from.to_string(), "2024-03-01");
        assert_eq!(p.prior.from.to_string(), "2024-02-01");
        assert_eq!(p.prior.to.to_string(), "2024-02-29");
        let args = SummaryArgs {
            from: Some(crate::cli::CalendarDateArg::decode("2024-03-10")),
            to: Some(crate::cli::CalendarDateArg::decode("2024-03-15")),
            ..SummaryArgs::default()
        };
        let p = args.periods(today)?;
        assert_eq!(p.prior.from.to_string(), "2024-03-04");
        assert_eq!(p.prior.to.to_string(), "2024-03-09");
        let p = SummaryArgs {
            to: Some(crate::cli::CalendarDateArg::decode("2025-01-01")),
            ..SummaryArgs::default()
        }
        .periods(today)?;
        assert_eq!(p.prior.from.to_string(), "2024-12-01");
        assert_eq!(p.prior.to.to_string(), "2024-12-31");
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
            to: Some(crate::cli::CalendarDateArg::decode("2024-03-15")),
            currency: Some(Currency::BHD.into()),
            ..SummaryArgs::default()
        };
        let report = compute(db.read(), "co", &args)?;
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
