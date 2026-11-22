//! Typed conversion from report tool input to the shared report command.

use serde_json::Value;

use super::dispatch::{calendar_arg, optional_text, text_list, unpaged};
use super::input_values::{CalendarDate, Text};
use super::inputs::{
    AgingInput, BalanceInput, BalanceSheetInput, BudgetVarianceInput, CashFlowInput,
    IncomeStatementInput, LedgerInput, ReconciliationInput, Report, ReportOptions, TaxPeriodInput,
    TrendInput, TrialBalanceInput, Vendor1099Input,
};
use crate::cli::{BudgetVarianceArgs, Cli, Command, CurrencyArg, ReportArgs, ReportCommand};
use crate::commands::{
    report,
    report_cash_flow::CashFlowArgs,
    report_periods::{CompareArg, PeriodArg},
    report_tax::{TaxArgs, VendorArgs},
    report_trend::TrendArgs,
};
use crate::db::Db;
use crate::error::CliError;

pub(super) fn execute(db: &Db, input: Report) -> Result<(Value, Option<Value>), CliError> {
    let command = Command::Report(ReportArgs {
        command: input.options.try_into()?,
        company: None,
    });
    let cli = Cli::for_mcp(command, input.chart, input.chart_all);
    let Command::Report(args) = &cli.command else {
        return Err(CliError::General("invalid report request".into()));
    };
    unpaged(report::execute_json(
        &cli,
        db,
        input.company.into_inner().as_str(),
        &args.command,
    ))
}

fn comparison(
    compare: Option<Text<CompareArg>>,
    has_period: bool,
    has_explanation: bool,
) -> Result<Option<CompareArg>, CliError> {
    if has_explanation && compare.is_none() {
        return Err(CliError::Usage("explain_change requires compare".into()));
    }
    if has_period && compare.is_some() {
        return Err(CliError::Usage(
            "period and compare are mutually exclusive".into(),
        ));
    }
    Ok(compare.map(Text::into_inner))
}

fn end_date(
    to: Option<Text<CalendarDate>>,
    as_of: Option<Text<CalendarDate>>,
) -> Result<Option<crate::cli::CalendarDateArg>, CliError> {
    match (to, as_of) {
        (Some(_), Some(_)) => Err(CliError::Usage("specify either as_of or to".into())),
        (date, None) | (None, date) => Ok(calendar_arg(date)),
    }
}

impl TryFrom<ReportOptions> for ReportCommand {
    type Error = CliError;

    fn try_from(options: ReportOptions) -> Result<Self, Self::Error> {
        match options {
            ReportOptions::Trend(input) => trend(input),
            ReportOptions::TrialBalance(input) => trial_balance(input),
            ReportOptions::Balance(input) => balance(input),
            ReportOptions::IncomeStatement(input) => income_statement(input),
            ReportOptions::BalanceSheet(input) => balance_sheet(input),
            ReportOptions::ScheduleC(input) => Ok(Self::ScheduleC(input.into())),
            ReportOptions::Vendor1099(input) => Ok(Self::Vendor1099(input.into())),
            ReportOptions::TaxSummary(input) => Ok(Self::TaxSummary {
                from: calendar_arg(input.from),
                to: calendar_arg(input.to),
                currency: input.currency.map(|currency| currency.into_inner().into()),
            }),
            ReportOptions::Ledger(input) => Ok(ledger(input)),
            ReportOptions::BudgetVariance(input) => budget_variance(input),
            ReportOptions::Aging(input) => Ok(aging(input)),
            ReportOptions::Reconciliation(input) => reconciliation(input),
            ReportOptions::CashFlow(input) => cash_flow(input),
        }
    }
}

fn trend(input: TrendInput) -> Result<ReportCommand, CliError> {
    Ok(ReportCommand::Trend(TrendArgs {
        metric: input.metric.into(),
        from: calendar_arg(input.from),
        to: calendar_arg(input.to),
        period: input.period.map_or(PeriodArg::Month, Into::into),
        currency: input.currency.map(|currency| currency.into_inner().into()),
        compare: comparison(input.compare, false, input.explain_change.is_some())?,
        explain_change: optional_text(input.explain_change),
    }))
}

fn trial_balance(input: TrialBalanceInput) -> Result<ReportCommand, CliError> {
    let compare = comparison(
        input.compare,
        input.period.is_some(),
        input.explain_change.is_some(),
    )?;
    Ok(ReportCommand::TrialBalance {
        from: calendar_arg(input.from),
        to: end_date(input.to, input.as_of)?,
        account_type: input.account_type,
        currency: input.currency.map(|currency| currency.into_inner().into()),
        period: input.period.map(Into::into),
        compare,
        explain_change: optional_text(input.explain_change),
    })
}

fn balance(input: BalanceInput) -> Result<ReportCommand, CliError> {
    Ok(ReportCommand::Balance {
        account: input.account.to_string(),
        from: calendar_arg(input.from),
        to: end_date(input.to, input.as_of)?,
        currency: input.currency.map(|currency| currency.into_inner().into()),
        compare: comparison(input.compare, false, input.explain_change.is_some())?,
        explain_change: optional_text(input.explain_change),
    })
}

fn income_statement(input: IncomeStatementInput) -> Result<ReportCommand, CliError> {
    let compare = comparison(
        input.compare,
        input.period.is_some(),
        input.explain_change.is_some(),
    )?;
    Ok(ReportCommand::IncomeStatement {
        from: calendar_arg(input.from),
        to: calendar_arg(input.to),
        currency: input.currency.map(|currency| currency.into_inner().into()),
        period: input.period.map(Into::into),
        compare,
        explain_change: optional_text(input.explain_change),
    })
}

fn balance_sheet(input: BalanceSheetInput) -> Result<ReportCommand, CliError> {
    Ok(ReportCommand::BalanceSheet {
        to: end_date(input.to, input.as_of)?,
        currency: input.currency.map(|currency| currency.into_inner().into()),
        compare: comparison(input.compare, false, input.explain_change.is_some())?,
        explain_change: optional_text(input.explain_change),
    })
}

impl From<TaxPeriodInput> for TaxArgs {
    fn from(input: TaxPeriodInput) -> Self {
        Self {
            from: calendar_arg(input.from),
            to: calendar_arg(input.to),
            currency: input.currency.map(|currency| currency.into_inner().into()),
            output_format: None,
        }
    }
}

impl From<Vendor1099Input> for VendorArgs {
    fn from(input: Vendor1099Input) -> Self {
        Self {
            period: TaxArgs {
                from: calendar_arg(input.from),
                to: calendar_arg(input.to),
                currency: input.currency.map(|currency| currency.into_inner().into()),
                output_format: None,
            },
            threshold: optional_text(input.threshold).unwrap_or_else(|| "600".into()),
        }
    }
}

fn ledger(input: LedgerInput) -> ReportCommand {
    ReportCommand::Ledger {
        account: text_list(input.account),
        from: calendar_arg(input.from),
        to: calendar_arg(input.to),
        currency: input.currency.map(|currency| currency.into_inner().into()),
        filter_metadata: input.filter_metadata,
    }
}

fn budget_variance(input: BudgetVarianceInput) -> Result<ReportCommand, CliError> {
    if input.month.is_some() && (input.from.is_some() || input.to.is_some()) {
        return Err(CliError::Usage("month conflicts with from and to".into()));
    }
    Ok(ReportCommand::BudgetVariance(BudgetVarianceArgs {
        year: input.year,
        month: input.month.map(|month| month.into_inner().into()),
        from: input.from.map(|month| month.into_inner().into()),
        to: input.to.map(|month| month.into_inner().into()),
        account_type: input.account_type,
        currency: input
            .currency
            .map(|currency| currency.into_inner().into())
            .or_else(|| {
                std::env::var("BEANKEEPER_CURRENCY")
                    .ok()
                    .map(|code| CurrencyArg::decode(&code))
            }),
        include_unbudgeted: input.include_unbudgeted,
    }))
}

fn aging(input: AgingInput) -> ReportCommand {
    ReportCommand::Aging(crate::cli::AgingCommandArgs {
        account: text_list(input.account.into_vec()),
        include_children: input.include_children,
        as_of: calendar_arg(input.as_of),
        buckets: input.buckets.unwrap_or_else(|| "30,60,90".into()),
        group_by_metadata: input.group_by_metadata,
        currency: input.currency.map(|currency| currency.into_inner().into()),
        method: input.method.map(Into::into).unwrap_or_default(),
        link_key: input.link_key,
        detail: input.detail,
        group_across_accounts: input.group_across_accounts,
        filter_metadata: input.filter_metadata,
    })
}

fn reconciliation(input: ReconciliationInput) -> Result<ReportCommand, CliError> {
    Ok(ReportCommand::Reconciliation(
        crate::cli::ReconciliationCommandArgs {
            mode: input.mode.map(Into::into).unwrap_or_default(),
            link_key: input.link_key,
            account: input.account.to_string(),
            to: end_date(input.to, input.as_of)?,
            statement_balance: optional_text(input.statement_balance),
            statement: input.statement,
            filter_metadata: input.filter_metadata,
            group_by_metadata: input.group_by_metadata,
            currency: input.currency.map(|currency| currency.into_inner().into()),
        },
    ))
}

fn cash_flow(input: CashFlowInput) -> Result<ReportCommand, CliError> {
    Ok(ReportCommand::CashFlow(CashFlowArgs {
        from: calendar_arg(input.from),
        to: calendar_arg(input.to),
        cash_account: text_list(input.cash_account),
        currency: input.currency.map(|currency| currency.into_inner().into()),
        compare: comparison(input.compare, false, input.explain_change.is_some())?,
        explain_change: optional_text(input.explain_change),
    }))
}
