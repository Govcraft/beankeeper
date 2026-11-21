//! Advisor-ready, read-only financial snapshot shared by CLI and MCP.

mod attention;
mod model;

use std::io::Write;

use chrono::{Datelike, Days, NaiveDate};
use clap::Args;
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::{Cli, OutputFormat, resolve_format};
use crate::db::{self, BalanceRow, connection::Db};
use crate::error::CliError;
use crate::output::json::{Envelope, EnvelopeError};

/// Options for `bk summary` and the `financial_summary` MCP tool.
#[derive(Debug, Clone, Default, Args, serde::Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SummaryArgs {
    /// First date, inclusive (YYYY-MM-DD). Defaults to the end date's month start.
    #[arg(long)]
    pub from: Option<String>,
    /// Last date, inclusive (YYYY-MM-DD). Defaults to today.
    #[arg(long)]
    pub to: Option<String>,
    /// First comparison date. Requires --compare-to.
    #[arg(long, requires = "compare_to")]
    pub compare_from: Option<String>,
    /// Last comparison date. Requires --compare-from.
    #[arg(long, requires = "compare_from")]
    pub compare_to: Option<String>,
    /// Currency code; required when the company's currencies are ambiguous.
    #[arg(long)]
    pub currency: Option<String>,
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

#[derive(Serialize)]
struct Periods {
    current: Period,
    prior: Period,
}

impl SummaryArgs {
    fn periods(&self, today: NaiveDate) -> Result<Periods, CliError> {
        let end =
            super::report_dates::DateWindow::resolve(None, self.to.as_deref(), false, today)?.to;
        let month_start = end.with_day(1).ok_or_else(date_error)?;
        let start = self.from.clone().unwrap_or_else(|| month_start.to_string());
        let window = super::report_dates::DateWindow::resolve(
            Some(&start),
            Some(&end.to_string()),
            false,
            today,
        )?;
        let current = Period {
            from: window.from.ok_or_else(date_error)?,
            to: window.to,
        };
        let prior = match (&self.compare_from, &self.compare_to) {
            (Some(from), Some(to)) => {
                let window =
                    super::report_dates::DateWindow::resolve(Some(from), Some(to), false, today)?;
                Period {
                    from: window.from.ok_or_else(date_error)?,
                    to: window.to,
                }
            }
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
    if matches!(row.account_type.as_str(), "asset" | "expense") {
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
    kind: &str,
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
    db::audit::atomically(db.conn(), || {
        db::companies::get_company(db.conn(), company)?;
        let periods = args.periods(chrono::Local::now().date_naive())?;
        let currency = super::report_currency::resolve_report_currency(
            db.conn(),
            company,
            args.currency.as_deref(),
        )?;
        let rows = db::compute_trial_balance(
            db.conn(),
            company,
            None,
            None,
            Some(&periods.current.to.to_string()),
            Some(currency.code()),
        )?;
        let cash = select_accounts(
            &rows,
            &args.cash_account,
            "asset",
            super::report_cash_flow::DEFAULT_CASH_NAME_KEYWORDS,
        )?;
        let receivables =
            select_accounts(&rows, &args.receivable_account, "asset", &["receivable"])?;
        let payables = select_accounts(&rows, &args.payable_account, "liability", &["payable"])?;
        let activity = |period: Period| {
            db::compute_trial_balance(
                db.conn(),
                company,
                None,
                Some(&period.from.to_string()),
                Some(&period.to.to_string()),
                Some(currency.code()),
            )
        };
        let current = activity(periods.current)?;
        let prior = activity(periods.prior)?;
        let comparison = model::comparison(&current, &prior)?;
        let movers = model::movers(&current, &prior)?;
        let attention = attention::compute(&attention::AttentionContext {
            conn: db.conn(),
            company,
            currency: currency.code(),
            as_of: periods.current.to,
            cash: &cash,
            receivables: &receivables,
            payables: &payables,
        })?;
        let mut view = model::desktop_view(company, currency, &periods);
        model::select_view_accounts(
            &mut view,
            [
                ("cash", &cash),
                ("receivable", &receivables),
                ("payable", &payables),
            ],
        );
        let standing = json!({
            "as_of":periods.current.to,
            "cash_on_hand":sum(rows.iter().filter(|row| cash.contains(&row.code)))?,
            "owed_to_you":sum(rows.iter().filter(|row| receivables.contains(&row.code)))?,
            "you_owe":sum(rows.iter().filter(|row| row.account_type == "liability"))?,
            "net_worth":sub(sum(rows.iter().filter(|row| row.account_type == "asset"))?, sum(rows.iter().filter(|row| row.account_type == "liability"))?)?,
            "cash_accounts":cash, "receivable_accounts":receivables, "payable_accounts":payables,
            "cash_accounts_identified":!cash.is_empty(), "receivable_accounts_identified":!receivables.is_empty(),
            "desktop_view":view,
        });
        let data = json!({
            "currency":currency.code(), "minor_units":currency.minor_units(),
            "periods":periods, "standing":standing,
            "comparison":{"series":comparison, "desktop_view":view},
            "biggest_movers":{"series":movers, "desktop_view":view},
            "needs_attention":{"items":attention, "desktop_view":view},
            "highlights":model::highlights(&standing, &comparison, &movers, currency),
            "desktop_view":view,
            "basis":{
                "activity":"Posted revenue and net expenses, including refunds; leftover is income minus spending, not cash flow.",
                "valuation":"Net worth uses ledger book values, not investment market prices.",
                "accounts":"Cash: asset names containing cash/bank/checking/savings; receivables: asset names containing receivable; payables: liability names containing payable. Explicit selectors replace inferred lists; an empty selection totals zero. You owe includes every liability.",
                "comparison":"Without --from: month to date versus the full prior calendar month. With --from: the immediately preceding equal-length interval. Explicit comparison dates override either default."
            }
        });
        serde_json::to_value(Envelope {
            ok: true,
            meta: cli.meta("summary", Some(company)).with_currency(currency),
            data: Some(data),
            error: None::<EnvelopeError>,
        })
        .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
    })
}

/// Render `bk summary` in plain words, JSON, or CSV.
///
/// # Errors
/// Returns summary validation, database, serialization or output errors.
pub fn run(cli: &Cli, company: &str, args: &SummaryArgs) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let envelope = execute_json(cli, &db, company, args)?;
    let rendered = match resolve_format(None, cli) {
        OutputFormat::Json => serde_json::to_string_pretty(&envelope)
            .map_err(|error| CliError::General(error.to_string()))?,
        OutputFormat::Table => model::plain(&envelope["data"]),
        OutputFormat::Csv => model::csv(&envelope["data"])?,
    };
    writeln!(std::io::stdout().lock(), "{rendered}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_handle_month_edges_leap_years_and_explicit_intervals() -> Result<(), CliError> {
        let today = NaiveDate::from_ymd_opt(2024, 3, 15).ok_or_else(date_error)?;
        let p = SummaryArgs::default().periods(today)?;
        assert_eq!(p.current.from.to_string(), "2024-03-01");
        assert_eq!(p.prior.from.to_string(), "2024-02-01");
        assert_eq!(p.prior.to.to_string(), "2024-02-29");
        let args = SummaryArgs {
            from: Some("2024-03-10".into()),
            to: Some("2024-03-15".into()),
            ..SummaryArgs::default()
        };
        let p = args.periods(today)?;
        assert_eq!(p.prior.from.to_string(), "2024-03-04");
        assert_eq!(p.prior.to.to_string(), "2024-03-09");
        let p = SummaryArgs {
            to: Some("2025-01-01".into()),
            ..SummaryArgs::default()
        }
        .periods(today)?;
        assert_eq!(p.prior.from.to_string(), "2024-12-01");
        assert_eq!(p.prior.to.to_string(), "2024-12-31");
        Ok(())
    }

    #[test]
    fn checked_money_rejects_overflow() {
        assert!(add(i64::MAX, 1).is_err());
        assert!(sub(i64::MIN, 1).is_err());
    }
}
