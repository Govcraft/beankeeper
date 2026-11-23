//! Shared report selections decoded by the CLI and MCP boundaries.

use beankeeper::types::{AccountType, Currency};
use chrono::{Month, NaiveDate};

use super::{AccountSelector, ChartRequest, DateRange, DateWindow, ReportRequest};
use crate::cli::{BudgetVarianceArgs, CalendarDateArg, ReportCommand, ReportMonth};
use crate::commands::{
    report_aging::AgingRequest,
    report_dates::resolve_date,
    report_periods::{CompareArg, PeriodArg},
    report_reconciliation::ReconciliationSelection,
    report_tax::{TaxArgs, VendorMinimum},
    report_trend::TrendMetric,
};
use crate::db::{
    BudgetCoverage, BudgetMonths,
    companies::CompanySlug,
    metadata_filter::{self, MetadataFilter},
};
use crate::error::CliError;
use crate::operation::Operation;
use crate::request::CurrencySetting;

/// A comparison and its optional account-level explanation.
#[derive(Debug)]
pub(crate) struct Comparison {
    pub(crate) compare: CompareArg,
    pub(crate) explain: Option<AccountSelector>,
}

#[derive(Debug, Default)]
pub(crate) struct ComparisonWindow(pub(super) Option<Comparison>);

impl TryFrom<(Option<CompareArg>, Option<AccountSelector>)> for ComparisonWindow {
    type Error = CliError;

    fn try_from(
        (compare, explain): (Option<CompareArg>, Option<AccountSelector>),
    ) -> Result<Self, Self::Error> {
        match (compare, explain) {
            (Some(compare), explain) => Ok(Self(Some(Comparison { compare, explain }))),
            (None, None) => Ok(Self::default()),
            (None, Some(_)) => Err(CliError::Usage("explain_change requires compare".into())),
        }
    }
}

/// Mutually exclusive running, calendar, and comparison windows.
#[derive(Debug, Default)]
pub(crate) enum ReportWindow {
    #[default]
    Single,
    Periods(PeriodArg),
    Compare(Comparison),
}

impl
    TryFrom<(
        Option<PeriodArg>,
        Option<CompareArg>,
        Option<AccountSelector>,
    )> for ReportWindow
{
    type Error = CliError;

    fn try_from(
        (period, compare, explain): (
            Option<PeriodArg>,
            Option<CompareArg>,
            Option<AccountSelector>,
        ),
    ) -> Result<Self, Self::Error> {
        let comparison = ComparisonWindow::try_from((compare, explain))?;
        match (period, comparison.0) {
            (None, None) => Ok(Self::Single),
            (Some(period), None) => Ok(Self::Periods(period)),
            (None, Some(comparison)) => Ok(Self::Compare(comparison)),
            (Some(_), Some(_)) => Err(CliError::Usage(
                "--period cannot be used with --compare".into(),
            )),
        }
    }
}

/// Date and currency selection common to financial statements.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReportScope {
    pub(in crate::commands) dates: DateWindow,
    pub(in crate::commands) currency: Option<Currency>,
}

impl TryFrom<(Option<NaiveDate>, Option<NaiveDate>, Option<Currency>)> for ReportScope {
    type Error = CliError;

    fn try_from(
        (from, to, currency): (Option<NaiveDate>, Option<NaiveDate>, Option<Currency>),
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            dates: (from, to).try_into()?,
            currency,
        })
    }
}

impl
    TryFrom<(
        Option<&CalendarDateArg>,
        Option<&CalendarDateArg>,
        Option<Currency>,
    )> for ReportScope
{
    type Error = CliError;

    fn try_from(
        (from, to, currency): (
            Option<&CalendarDateArg>,
            Option<&CalendarDateArg>,
            Option<Currency>,
        ),
    ) -> Result<Self, Self::Error> {
        Self::try_from((
            from.map(|date| resolve_date("--from", date)).transpose()?,
            to.map(|date| resolve_date("--to", date)).transpose()?,
            currency,
        ))
    }
}

impl TryFrom<ReportScope> for DateRange {
    type Error = CliError;

    fn try_from(scope: ReportScope) -> Result<Self, Self::Error> {
        scope.dates.try_into()
    }
}

impl From<ReportScope> for Option<Currency> {
    fn from(scope: ReportScope) -> Self {
        scope.currency
    }
}

/// Month choices for budget variance, normalized to a validated range.
pub(crate) struct VarianceMonths(BudgetMonths);

impl TryFrom<(i32, Option<Month>, Option<Month>, Option<Month>)> for VarianceMonths {
    type Error = CliError;

    fn try_from(
        (year, month, from, to): (i32, Option<Month>, Option<Month>, Option<Month>),
    ) -> Result<Self, Self::Error> {
        let (from, to) = match (month, from, to) {
            (Some(month), None, None) => (month, month),
            (None, from, to) => (
                from.unwrap_or(Month::January),
                to.unwrap_or(Month::December),
            ),
            _ => return Err(CliError::Usage("month conflicts with from and to".into())),
        };
        BudgetMonths::new(year, from, to).map(Self)
    }
}

impl TryFrom<&BudgetVarianceArgs> for VarianceMonths {
    type Error = CliError;

    fn try_from(args: &BudgetVarianceArgs) -> Result<Self, Self::Error> {
        let month = args
            .month
            .map(ReportMonth::resolve)
            .transpose()
            .map_err(|number| CliError::Validation(format!("month must be 1-12, got {number}")))?;
        let endpoints = |month: Option<ReportMonth>| {
            month
                .map(ReportMonth::resolve)
                .transpose()
                .map_err(|_| CliError::Validation("from/to months must be 1-12".into()))
        };
        Self::try_from((args.year, month, endpoints(args.from)?, endpoints(args.to)?))
    }
}

impl From<VarianceMonths> for BudgetMonths {
    fn from(months: VarianceMonths) -> Self {
        months.0
    }
}

/// Validated inputs to each financial report.
pub(crate) enum ReportQuery {
    TrialBalance {
        scope: ReportScope,
        account_type: Option<AccountType>,
        window: ReportWindow,
    },
    Balance {
        scope: ReportScope,
        account: AccountSelector,
        window: ComparisonWindow,
    },
    IncomeStatement {
        scope: ReportScope,
        window: ReportWindow,
    },
    BalanceSheet {
        scope: ReportScope,
        window: ComparisonWindow,
    },
    TaxSummary {
        dates: DateRange,
        currency: Option<Currency>,
    },
    ScheduleC {
        dates: DateRange,
        currency: Option<Currency>,
    },
    Vendor1099 {
        dates: DateRange,
        currency: Option<Currency>,
        threshold: VendorMinimum,
    },
    Ledger {
        scope: ReportScope,
        accounts: Vec<AccountSelector>,
        filters: Vec<MetadataFilter>,
    },
    BudgetVariance {
        months: BudgetMonths,
        account_type: Option<AccountType>,
        currency: Option<Currency>,
        coverage: BudgetCoverage,
    },
    Trend {
        scope: ReportScope,
        period: PeriodArg,
        metric: TrendMetric,
        window: ComparisonWindow,
    },
    CashFlow {
        scope: ReportScope,
        accounts: Vec<AccountSelector>,
        window: ComparisonWindow,
    },
    Aging(AgingRequest),
    Reconciliation(ReconciliationSelection),
}

impl ReportQuery {
    pub(super) const fn operation(&self) -> Operation {
        match self {
            Self::TrialBalance { window, .. } => match window {
                ReportWindow::Single => Operation::ReportTrialBalance,
                ReportWindow::Periods(_) => Operation::ReportTrialBalancePeriods,
                ReportWindow::Compare(_) => Operation::ReportTrialBalanceCompare,
            },
            Self::IncomeStatement { window, .. } => match window {
                ReportWindow::Single => Operation::ReportIncomeStatement,
                ReportWindow::Periods(_) => Operation::ReportIncomeStatementPeriods,
                ReportWindow::Compare(_) => Operation::ReportIncomeStatementCompare,
            },
            Self::Balance { .. } => Operation::ReportBalance,
            Self::BalanceSheet { .. } => Operation::ReportBalanceSheet,
            Self::TaxSummary { .. } => Operation::ReportTaxSummary,
            Self::ScheduleC { .. } => Operation::ReportScheduleC,
            Self::Vendor1099 { .. } => Operation::ReportVendor1099,
            Self::Ledger { .. } => Operation::ReportLedger,
            Self::BudgetVariance { .. } => Operation::ReportBudgetVariance,
            Self::Trend { .. } => Operation::ReportTrend,
            Self::CashFlow { .. } => Operation::ReportCashFlow,
            Self::Aging(_) => Operation::ReportAging,
            Self::Reconciliation(_) => Operation::ReportReconciliation,
        }
    }

    pub(super) const fn supports_chart(&self) -> bool {
        matches!(
            self,
            Self::Trend { .. }
                | Self::CashFlow { .. }
                | Self::IncomeStatement { .. }
                | Self::TrialBalance {
                    window: ReportWindow::Periods(_) | ReportWindow::Compare(_),
                    ..
                }
        )
    }
}

impl From<bool> for BudgetCoverage {
    fn from(include_unbudgeted: bool) -> Self {
        if include_unbudgeted {
            Self::IncludeUnbudgeted
        } else {
            Self::Budgeted
        }
    }
}

impl TryFrom<(&CompanySlug, &ReportCommand, (bool, bool))> for ReportRequest {
    type Error = CliError;

    fn try_from(
        (company, command, chart): (&CompanySlug, &ReportCommand, (bool, bool)),
    ) -> Result<Self, Self::Error> {
        Self::try_from((company, command, ChartRequest::try_from(chart)?))
    }
}

impl TryFrom<(&CompanySlug, &ReportCommand, ChartRequest)> for ReportRequest {
    type Error = CliError;

    fn try_from(
        (company, command, chart): (&CompanySlug, &ReportCommand, ChartRequest),
    ) -> Result<Self, Self::Error> {
        Self::new(company.clone(), command.try_into()?, chart)
    }
}

struct TaxSelection(DateRange, Option<Currency>);

impl TryFrom<&TaxArgs> for TaxSelection {
    type Error = CliError;

    fn try_from(args: &TaxArgs) -> Result<Self, Self::Error> {
        let scope = ReportScope::try_from((args.from.as_ref(), args.to.as_ref(), args.currency))?;
        Ok(Self(scope.try_into()?, scope.into()))
    }
}

impl TryFrom<&ReportCommand> for ReportQuery {
    type Error = CliError;

    fn try_from(command: &ReportCommand) -> Result<Self, Self::Error> {
        let scope = |from: &Option<_>, to: &Option<_>, currency: &Option<_>| {
            ReportScope::try_from((from.as_ref(), to.as_ref(), *currency))
        };
        let account = |value: &Option<String>| value.clone().map(AccountSelector::from);
        let accounts =
            |values: &[String]| values.iter().cloned().map(AccountSelector::from).collect();
        Ok(match command {
            ReportCommand::TrialBalance {
                from,
                to,
                currency,
                account_type,
                period,
                compare,
                explain_change,
            } => Self::TrialBalance {
                scope: scope(from, to, currency)?,
                account_type: *account_type,
                window: (*period, *compare, account(explain_change)).try_into()?,
            },
            ReportCommand::Balance {
                from,
                to,
                currency,
                account: code,
                compare,
                explain_change,
            } => Self::Balance {
                scope: scope(from, to, currency)?,
                account: code.clone().into(),
                window: (*compare, account(explain_change)).try_into()?,
            },
            ReportCommand::IncomeStatement {
                from,
                to,
                currency,
                period,
                compare,
                explain_change,
            } => Self::IncomeStatement {
                scope: scope(from, to, currency)?,
                window: (*period, *compare, account(explain_change)).try_into()?,
            },
            ReportCommand::BalanceSheet {
                to,
                currency,
                compare,
                explain_change,
            } => Self::BalanceSheet {
                scope: scope(&None, to, currency)?,
                window: (*compare, account(explain_change)).try_into()?,
            },
            ReportCommand::TaxSummary { from, to, currency } => {
                let selected = scope(from, to, currency)?;
                Self::TaxSummary {
                    dates: selected.dates.try_into()?,
                    currency: selected.currency,
                }
            }
            ReportCommand::ScheduleC(args) => {
                let TaxSelection(dates, currency) = args.try_into()?;
                Self::ScheduleC { dates, currency }
            }
            ReportCommand::Vendor1099(args) => {
                let TaxSelection(dates, currency) = (&args.period).try_into()?;
                Self::Vendor1099 {
                    dates,
                    currency,
                    threshold: args.threshold.clone().unwrap_or_default(),
                }
            }
            ReportCommand::Ledger {
                account,
                from,
                to,
                currency,
                filter_metadata,
            } => Self::Ledger {
                scope: scope(from, to, currency)?,
                accounts: accounts(account),
                filters: metadata_filter::parse_filters(filter_metadata)?,
            },
            ReportCommand::BudgetVariance(args) => return args.try_into(),
            ReportCommand::Trend(args) => Self::Trend {
                scope: scope(&args.from, &args.to, &args.currency)?,
                period: args.period.unwrap_or_default(),
                metric: args.metric,
                window: (args.compare, account(&args.explain_change)).try_into()?,
            },
            ReportCommand::CashFlow(args) => Self::CashFlow {
                scope: scope(&args.from, &args.to, &args.currency)?,
                accounts: accounts(&args.cash_account),
                window: (args.compare, account(&args.explain_change)).try_into()?,
            },
            ReportCommand::Aging(args) => Self::Aging(args.try_into()?),
            ReportCommand::Reconciliation(args) => Self::Reconciliation(args.try_into()?),
        })
    }
}

impl TryFrom<&BudgetVarianceArgs> for ReportQuery {
    type Error = CliError;

    fn try_from(args: &BudgetVarianceArgs) -> Result<Self, Self::Error> {
        Ok(Self::BudgetVariance {
            months: VarianceMonths::try_from(args)?.into(),
            account_type: args.account_type,
            currency: CurrencySetting::from(args.currency).try_into()?,
            coverage: BudgetCoverage::from(args.include_unbudgeted),
        })
    }
}
