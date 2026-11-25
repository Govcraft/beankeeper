//! Output formatting: table, JSON, and CSV rendering.
//!
//! Each sub-module renders a specific output format. The [`should_use_color`]
//! function determines whether ANSI colour codes should be emitted based on
//! terminal state and user flags.

pub(crate) mod amounts;
pub(crate) mod chart;
pub mod csv;
pub mod json;
pub mod report_periods;
pub mod statements;
pub mod table;

use std::io::IsTerminal;

use beankeeper::types::Amount;

use crate::db::BalanceRow;
use crate::error::CliError;

struct TrialBalanceTotals {
    debits: i64,
    credits: i64,
}

impl TrialBalanceTotals {
    fn from_rows(balances: &[BalanceRow]) -> Result<Self, CliError> {
        let mut debits = Amount::ZERO;
        let mut credits = Amount::ZERO;
        for row in balances {
            debits = debits
                .checked_add(Amount::from(row.debit_total))
                .ok_or_else(trial_balance_overflow)?;
            credits = credits
                .checked_add(Amount::from(row.credit_total))
                .ok_or_else(trial_balance_overflow)?;
        }
        Ok(Self {
            debits: i64::try_from(debits.minor_units()).map_err(|_| trial_balance_overflow())?,
            credits: i64::try_from(credits.minor_units()).map_err(|_| trial_balance_overflow())?,
        })
    }
}

fn trial_balance_overflow() -> CliError {
    CliError::Validation("trial balance exceeds the supported minor-unit range".into())
}

/// Determine if colors should be used based on environment and flags.
///
/// Colors are disabled when any of the following are true:
/// - `no_color_flag` is `true` (`--no-color` was passed)
/// - The `NO_COLOR` environment variable is set (any value, per <https://no-color.org>)
/// - `TERM=dumb`
/// - stdout is not a terminal (piped / redirected output)
#[must_use]
pub fn should_use_color(no_color_flag: bool) -> bool {
    if no_color_flag {
        return false;
    }
    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }
    if std::env::var("TERM").ok().as_deref() == Some("dumb") {
        return false;
    }
    std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trial_balance_renderers_reject_totals_outside_the_supported_range() {
        let accounts = [
            BalanceRow {
                code: "1000".into(),
                name: "Cash".into(),
                account_type: beankeeper::types::AccountType::Asset,
                debit_total: i64::MAX,
                credit_total: 0,
            },
            BalanceRow {
                code: "1100".into(),
                name: "Receivables".into(),
                account_type: beankeeper::types::AccountType::Asset,
                debit_total: 1,
                credit_total: 0,
            },
        ];
        let meta = json::meta(
            crate::operation::Operation::ReportTrialBalance,
            Some("acme"),
        );
        assert!(matches!(
            json::render_trial_balance(&accounts, beankeeper::types::Currency::USD, meta),
            Err(CliError::Validation(_))
        ));
        assert!(matches!(
            table::render_trial_balance(&accounts, beankeeper::types::Currency::USD, false, None),
            Err(CliError::Validation(_))
        ));
    }

    #[test]
    fn no_color_flag_disables_color() {
        assert!(!should_use_color(true));
    }
}

/// A navigation target for Desktop hosts that expose the transaction register.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DesktopRegister {
    view_id: &'static str,
    url: String,
}

impl DesktopRegister {
    pub(crate) fn new(
        company: &str,
        currency: beankeeper::types::Currency,
        account: Option<&str>,
        dates: (Option<chrono::NaiveDate>, Option<chrono::NaiveDate>),
    ) -> Self {
        use std::fmt::Write as _;

        let company = crate::commands::report_compare::encode(company);
        let mut url = format!("/transactions?company={company}&currency={currency}");
        if let Some(code) = account {
            let account = crate::commands::report_compare::encode(code);
            let _ = write!(url, "&account={account}");
        }
        for (key, date) in [("from", dates.0), ("to", dates.1)] {
            if let Some(date) = date {
                let _ = write!(url, "&{key}={date}");
            }
        }
        Self {
            view_id: "transactions",
            url,
        }
    }
}

/// Chartable delta and register target for a statement import settlement.
#[derive(serde::Serialize)]
pub(crate) struct ImportSettlement {
    settlement: crate::db::holds::SettlementLink,
    #[serde(serialize_with = "json::serialize_display")]
    settlement_currency: beankeeper::types::Currency,
    amount_delta_display: String,
    desktop_view: DesktopRegister,
}

impl ImportSettlement {
    pub(crate) fn new(
        settlement: crate::db::holds::SettlementLink,
        company: &str,
        account: Option<&str>,
        currency: beankeeper::types::Currency,
        date: chrono::NaiveDate,
    ) -> Self {
        let delta =
            beankeeper::types::Money::from_minor(i128::from(settlement.amount_delta()), currency);
        Self {
            settlement,
            settlement_currency: currency,
            amount_delta_display: amounts::format_money(delta),
            desktop_view: DesktopRegister::new(
                company,
                currency,
                account,
                (Some(date), Some(date)),
            ),
        }
    }
}
