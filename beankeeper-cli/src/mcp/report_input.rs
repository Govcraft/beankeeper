//! Typed conversion from report tool input to the shared report command.

use serde_json::Value;

use super::dispatch::{optional_text, text_list, unpaged};
use super::input_values::{Month, Text};
use super::inputs::{
    AgingInput, BalanceInput, BalanceSheetInput, BudgetVarianceInput, CashFlowInput,
    IncomeStatementInput, LedgerInput, ReconciliationInput, Report, ReportOptions, TaxPeriodInput,
    TrendInput, TrialBalanceInput, Vendor1099Input,
};
use crate::cli::{Cli, Command, ReportArgs, ReportCommand};
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
                from: optional_text(input.from),
                to: optional_text(input.to),
                currency: optional_text(input.currency),
            }),
            ReportOptions::Ledger(input) => Ok(ledger(input)),
            ReportOptions::BudgetVariance(input) => budget_variance(input),
            ReportOptions::Aging(input) => aging(input),
            ReportOptions::Reconciliation(input) => Ok(reconciliation(input)),
            ReportOptions::CashFlow(input) => cash_flow(input),
        }
    }
}

fn trend(input: TrendInput) -> Result<ReportCommand, CliError> {
    Ok(ReportCommand::Trend(TrendArgs {
        metric: input.metric.into(),
        from: optional_text(input.from),
        to: optional_text(input.to),
        period: input.period.map_or(PeriodArg::Month, Into::into),
        currency: optional_text(input.currency),
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
        from: optional_text(input.from),
        to: optional_text(input.to),
        account_type: input.account_type.map(Into::into),
        currency: optional_text(input.currency),
        period: input.period.map(Into::into),
        compare,
        explain_change: optional_text(input.explain_change),
    })
}

fn balance(input: BalanceInput) -> Result<ReportCommand, CliError> {
    Ok(ReportCommand::Balance {
        account: input.account.to_string(),
        from: optional_text(input.from),
        to: optional_text(input.to),
        currency: optional_text(input.currency),
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
        from: optional_text(input.from),
        to: optional_text(input.to),
        currency: optional_text(input.currency),
        period: input.period.map(Into::into),
        compare,
        explain_change: optional_text(input.explain_change),
    })
}

fn balance_sheet(input: BalanceSheetInput) -> Result<ReportCommand, CliError> {
    Ok(ReportCommand::BalanceSheet {
        to: optional_text(input.to),
        currency: optional_text(input.currency),
        compare: comparison(input.compare, false, input.explain_change.is_some())?,
        explain_change: optional_text(input.explain_change),
    })
}

impl From<TaxPeriodInput> for TaxArgs {
    fn from(input: TaxPeriodInput) -> Self {
        Self {
            from: optional_text(input.from),
            to: optional_text(input.to),
            currency: optional_text(input.currency),
            output_format: None,
        }
    }
}

impl From<Vendor1099Input> for VendorArgs {
    fn from(input: Vendor1099Input) -> Self {
        Self {
            period: TaxArgs {
                from: optional_text(input.from),
                to: optional_text(input.to),
                currency: optional_text(input.currency),
                output_format: None,
            },
            threshold: optional_text(input.threshold).unwrap_or_else(|| "600".into()),
        }
    }
}

fn ledger(input: LedgerInput) -> ReportCommand {
    ReportCommand::Ledger {
        account: text_list(input.account),
        from: optional_text(input.from),
        to: optional_text(input.to),
        currency: optional_text(input.currency),
        filter_metadata: input.filter_metadata,
    }
}

fn report_month(month: Option<&Month>) -> Result<Option<i32>, CliError> {
    month
        .map(|month| i32::try_from(month.get()))
        .transpose()
        .map_err(|error| CliError::Validation(error.to_string()))
}

fn budget_variance(input: BudgetVarianceInput) -> Result<ReportCommand, CliError> {
    if input.month.is_some() && (input.from.is_some() || input.to.is_some()) {
        return Err(CliError::Usage("month conflicts with from and to".into()));
    }
    Ok(ReportCommand::BudgetVariance {
        year: input.year,
        month: report_month(input.month.as_ref())?,
        from: report_month(input.from.as_ref())?,
        to: report_month(input.to.as_ref())?,
        account_type: input.account_type.map(Into::into),
        currency: optional_text(input.currency)
            .or_else(|| std::env::var("BEANKEEPER_CURRENCY").ok()),
        include_unbudgeted: input.include_unbudgeted,
    })
}

fn aging(input: AgingInput) -> Result<ReportCommand, CliError> {
    if input.account.is_empty() {
        return Err(CliError::Usage(
            "aging requires at least one account".into(),
        ));
    }
    Ok(ReportCommand::Aging {
        account: text_list(input.account),
        include_children: input.include_children,
        as_of: optional_text(input.as_of),
        buckets: input.buckets.unwrap_or_else(|| "30,60,90".into()),
        group_by_metadata: input.group_by_metadata,
        currency: optional_text(input.currency),
        method: input.method.map(Into::into).unwrap_or_default(),
        link_key: input.link_key,
        detail: input.detail,
        group_across_accounts: input.group_across_accounts,
        filter_metadata: input.filter_metadata,
    })
}

fn reconciliation(input: ReconciliationInput) -> ReportCommand {
    ReportCommand::Reconciliation {
        mode: input.mode.map(Into::into).unwrap_or_default(),
        link_key: input.link_key,
        account: input.account.to_string(),
        to: optional_text(input.to),
        statement_balance: optional_text(input.statement_balance),
        statement: input.statement,
        filter_metadata: input.filter_metadata,
        group_by_metadata: input.group_by_metadata,
        currency: optional_text(input.currency),
    }
}

fn cash_flow(input: CashFlowInput) -> Result<ReportCommand, CliError> {
    Ok(ReportCommand::CashFlow(CashFlowArgs {
        from: optional_text(input.from),
        to: optional_text(input.to),
        cash_account: text_list(input.cash_account),
        currency: optional_text(input.currency),
        compare: comparison(input.compare, false, input.explain_change.is_some())?,
        explain_change: optional_text(input.explain_change),
    }))
}
