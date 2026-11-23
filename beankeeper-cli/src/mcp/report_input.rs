//! Convert decoded MCP report, summary and audit fields into ledger requests.

use chrono::NaiveDate;

use super::input_values::{CalendarDate, Month, Text};
use super::inputs::{
    AgingInput, AgingMethodArg, AuditLog, BudgetVarianceInput, FinancialSummary,
    PeriodArg as WirePeriodArg, ReconciliationInput, ReconciliationModeArg, Report, ReportOptions,
    TaxPeriodInput, TrendMetricArg,
};
use crate::cli::{ActorFilterArg, ReconciliationMode};
use crate::commands::{
    audit::AuditQuery,
    report::{
        AccountSelector, ChartRequest, ReportQuery, ReportRequest, ReportScope, VarianceMonths,
    },
    report_aging::{AgingRequest, AgingSelection, MatchMethod},
    report_periods::PeriodArg,
    report_reconciliation::{ReconciliationOptions, ReconciliationSelection},
    report_trend::TrendMetric,
    summary::{SummaryAccounts, SummaryDates, SummaryRequest},
};
use crate::db::metadata_filter::parse_filters;
use crate::error::CliError;

fn date(value: Option<Text<CalendarDate>>) -> Option<NaiveDate> {
    value.map(|value| value.into_inner().date())
}

fn account(value: Text<beankeeper::types::AccountCode>) -> AccountSelector {
    value.into_inner().into()
}

fn accounts(values: Vec<Text<beankeeper::types::AccountCode>>) -> Vec<AccountSelector> {
    values.into_iter().map(account).collect()
}

impl From<WirePeriodArg> for PeriodArg {
    fn from(period: WirePeriodArg) -> Self {
        match period {
            WirePeriodArg::Month => Self::Month,
            WirePeriodArg::Quarter => Self::Quarter,
            WirePeriodArg::Year => Self::Year,
        }
    }
}

impl From<TrendMetricArg> for TrendMetric {
    fn from(metric: TrendMetricArg) -> Self {
        match metric {
            TrendMetricArg::Spending => Self::Spending,
            TrendMetricArg::NetWorth => Self::NetWorth,
        }
    }
}

impl From<AgingMethodArg> for MatchMethod {
    fn from(method: AgingMethodArg) -> Self {
        match method {
            AgingMethodArg::Fifo => Self::Fifo,
            AgingMethodArg::Specific => Self::Specific,
        }
    }
}

impl From<ReconciliationModeArg> for ReconciliationMode {
    fn from(mode: ReconciliationModeArg) -> Self {
        match mode {
            ReconciliationModeArg::Bank => Self::Bank,
            ReconciliationModeArg::Settlement => Self::Settlement,
        }
    }
}

impl TryFrom<Report> for ReportRequest {
    type Error = CliError;

    fn try_from(input: Report) -> Result<Self, Self::Error> {
        Self::new(
            input.company.into_inner(),
            input.options.try_into()?,
            (input.chart, input.chart_all).try_into()?,
        )
    }
}

impl TryFrom<TaxPeriodInput> for ReportScope {
    type Error = CliError;

    fn try_from(input: TaxPeriodInput) -> Result<Self, Self::Error> {
        Self::try_from((date(input.from), date(input.to), input.currency))
    }
}

impl TryFrom<ReportOptions> for ReportQuery {
    type Error = CliError;

    fn try_from(options: ReportOptions) -> Result<Self, Self::Error> {
        let scope = |from, to, currency: Option<beankeeper::types::Currency>| {
            ReportScope::try_from((date(from), date(to), currency))
        };
        Ok(match options {
            ReportOptions::TrialBalance(input) => Self::TrialBalance {
                scope: scope(input.from, input.to, input.currency)?,
                account_type: input.account_type,
                window: (
                    input.period.map(Into::into),
                    input.compare.map(Text::into_inner),
                    input.explain_change.map(account),
                )
                    .try_into()?,
            },
            ReportOptions::Balance(input) => Self::Balance {
                scope: scope(input.from, input.to, input.currency)?,
                account: account(input.account),
                window: (
                    input.compare.map(Text::into_inner),
                    input.explain_change.map(account),
                )
                    .try_into()?,
            },
            ReportOptions::IncomeStatement(input) => Self::IncomeStatement {
                scope: scope(input.from, input.to, input.currency)?,
                window: (
                    input.period.map(Into::into),
                    input.compare.map(Text::into_inner),
                    input.explain_change.map(account),
                )
                    .try_into()?,
            },
            ReportOptions::BalanceSheet(input) => Self::BalanceSheet {
                scope: scope(None, input.to, input.currency)?,
                window: (
                    input.compare.map(Text::into_inner),
                    input.explain_change.map(account),
                )
                    .try_into()?,
            },
            ReportOptions::TaxSummary(input) => {
                let scope = ReportScope::try_from(input)?;
                Self::TaxSummary {
                    dates: scope.try_into()?,
                    currency: scope.into(),
                }
            }
            ReportOptions::ScheduleC(input) => {
                let scope = ReportScope::try_from(input)?;
                Self::ScheduleC {
                    dates: scope.try_into()?,
                    currency: scope.into(),
                }
            }
            ReportOptions::Vendor1099(input) => {
                let scope = scope(input.from, input.to, input.currency)?;
                let threshold = input
                    .threshold
                    .map(|amount| amount.into_inner().try_into())
                    .transpose()?
                    .unwrap_or_default();
                Self::Vendor1099 {
                    dates: scope.try_into()?,
                    currency: scope.into(),
                    threshold,
                }
            }
            ReportOptions::Ledger(input) => Self::Ledger {
                scope: scope(input.from, input.to, input.currency)?,
                accounts: accounts(input.account),
                filters: parse_filters(&input.filter_metadata)?,
            },
            ReportOptions::BudgetVariance(input) => return input.try_into(),
            ReportOptions::Trend(input) => Self::Trend {
                scope: scope(input.from, input.to, input.currency)?,
                period: input.period.map(Into::into).unwrap_or_default(),
                metric: input.metric.into(),
                window: (
                    input.compare.map(Text::into_inner),
                    input.explain_change.map(account),
                )
                    .try_into()?,
            },
            ReportOptions::CashFlow(input) => Self::CashFlow {
                scope: scope(input.from, input.to, input.currency)?,
                accounts: accounts(input.cash_account),
                window: (
                    input.compare.map(Text::into_inner),
                    input.explain_change.map(account),
                )
                    .try_into()?,
            },
            ReportOptions::Aging(input) => Self::Aging(input.try_into()?),
            ReportOptions::Reconciliation(input) => Self::Reconciliation(input.try_into()?),
        })
    }
}

impl TryFrom<BudgetVarianceInput> for ReportQuery {
    type Error = CliError;

    fn try_from(input: BudgetVarianceInput) -> Result<Self, Self::Error> {
        Ok(Self::BudgetVariance {
            months: VarianceMonths::try_from((
                input.year,
                input.month.map(Month::into_inner),
                input.from.map(Month::into_inner),
                input.to.map(Month::into_inner),
            ))?
            .into(),
            account_type: input.account_type,
            currency: crate::request::CurrencySetting::from(input.currency).try_into()?,
            coverage: input.include_unbudgeted.into(),
        })
    }
}

impl TryFrom<AgingInput> for AgingRequest {
    type Error = CliError;

    fn try_from(input: AgingInput) -> Result<Self, Self::Error> {
        AgingSelection {
            accounts: input.account.map(account),
            include_children: input.include_children,
            as_of: date(input.as_of),
            buckets: input.buckets,
            group_by: input.group_by_metadata,
            currency: input.currency,
            method: input.method.map(Into::into).unwrap_or_default(),
            link_key: input.link_key,
            detail: input.detail,
            across: input.group_across_accounts,
            filters: input.filter_metadata,
        }
        .try_into()
    }
}

impl TryFrom<ReconciliationInput> for ReconciliationSelection {
    type Error = CliError;

    fn try_from(input: ReconciliationInput) -> Result<Self, Self::Error> {
        ReconciliationOptions {
            mode: input.mode.map(Into::into).unwrap_or_default(),
            account: account(input.account),
            as_of: date(input.to),
            statement_balance: input
                .statement_balance
                .map(|amount| amount.into_inner().into()),
            statement: input.statement,
            link_key: input.link_key,
            filters: input.filter_metadata,
            group: input.group_by_metadata,
            currency: input.currency,
        }
        .try_into()
    }
}

impl TryFrom<FinancialSummary> for SummaryRequest {
    type Error = CliError;

    fn try_from(input: FinancialSummary) -> Result<Self, Self::Error> {
        let options = input.options;
        Self::new(
            input.company.into_inner(),
            SummaryDates::try_from([
                date(options.from),
                date(options.to),
                date(options.compare_from),
                date(options.compare_to),
            ])?,
            options.currency,
            SummaryAccounts {
                cash: accounts(options.cash_account),
                receivables: accounts(options.receivable_account),
                payables: accounts(options.payable_account),
            },
            ChartRequest::try_from((input.chart, input.chart_all))?,
        )
    }
}

impl TryFrom<AuditLog> for AuditQuery {
    type Error = CliError;

    fn try_from(input: AuditLog) -> Result<Self, Self::Error> {
        Ok(Self {
            actor: ActorFilterArg::resolve_optional(input.actor.as_ref())?.cloned(),
            agent: input
                .agent
                .map(|value| value.into_inner().as_str().to_owned()),
            company: input.company.map(Text::into_inner),
            entity: input.entity,
            action: input.action,
            limit: input.limit.unwrap_or_default(),
            offset: input.offset.unwrap_or_default(),
        })
    }
}
