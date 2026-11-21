//! Canonical command identities and authorization requirements.

use crate::capability::Capability;
use crate::error::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permission {
    Capability(Capability),
    ProposalList,
    ProposalReject,
    Transport,
}

impl Permission {
    pub(crate) fn primary(self) -> Result<Capability, CliError> {
        match self {
            Self::Capability(capability) => Ok(capability),
            Self::ProposalList => Ok(Capability::Read),
            Self::ProposalReject => Ok(Capability::Propose),
            Self::Transport => Err(CliError::Usage(
                "operation has no capability mapping".into(),
            )),
        }
    }
}

macro_rules! permission {
    (ProposalList) => {
        Permission::ProposalList
    };
    (ProposalReject) => {
        Permission::ProposalReject
    };
    (Transport) => {
        Permission::Transport
    };
    ($capability:ident) => {
        Permission::Capability(Capability::$capability)
    };
}

macro_rules! operations {
    ($($operation:ident => ($name:literal, $permission:ident)),+ $(,)?) => {
        /// A recognized ledger command or MCP tool.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[non_exhaustive]
        pub enum Operation {
            $(#[doc = concat!("The `", $name, "` operation.")] $operation),+
        }

        impl Operation {
            /// The command name emitted in JSON metadata.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self { $(Self::$operation => $name),+ }
            }

            pub(crate) const fn permission(self) -> Permission {
                match self { $(Self::$operation => permission!($permission)),+ }
            }
        }

        impl std::str::FromStr for Operation {
            type Err = CliError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                let normalized;
                let name = if raw.contains('_') {
                    normalized = raw.replace('_', ".");
                    normalized.as_str()
                } else {
                    raw
                };
                match name {
                    $($name => Ok(Self::$operation),)+
                    "financial.summary" => Ok(Self::Summary),
                    "report.1099-nec" => Ok(Self::ReportVendor1099),
                    _ => Err(CliError::Usage("operation has no capability mapping".into())),
                }
            }
        }
    };
}

operations! {
    Summary => ("summary", Read),
    PeriodList => ("period.list", Read),
    CompanyList => ("company.list", Read),
    CompanyShow => ("company.show", Read),
    AccountList => ("account.list", Read),
    AccountShow => ("account.show", Read),
    TxnList => ("txn.list", Read),
    TxnShow => ("txn.show", Read),
    SecurityList => ("security.list", Read),
    InvestmentLots => ("investment.lots", Read),
    InvestmentHoldings => ("investment.holdings", Read),
    ScheduleList => ("schedule.list", Read),
    RuleList => ("rule.list", Read),
    BudgetList => ("budget.list", Read),
    LoanSchedule => ("loan.schedule", Read),
    Export => ("export", Read),
    Verify => ("verify", Read),
    AuditLog => ("audit.log", Read),
    TxnProposals => ("txn.proposals", ProposalList),
    AgentLimitShow => ("agent.limit.show", Read),
    AgentList => ("agent.list", Read),
    PolicyShow => ("policy.show", Read),
    Report => ("report", Read),
    ReportAging => ("report.aging", Read),
    ReportBalance => ("report.balance", Read),
    ReportBalanceSheet => ("report.balance-sheet", Read),
    ReportBudgetVariance => ("report.budget-variance", Read),
    ReportCashFlow => ("report.cash-flow", Read),
    ReportIncomeStatement => ("report.income-statement", Read),
    ReportIncomeStatementCompare => ("report.income-statement.compare", Read),
    ReportIncomeStatementPeriods => ("report.income-statement.periods", Read),
    ReportLedger => ("report.ledger", Read),
    ReportReconciliation => ("report.reconciliation", Read),
    ReportScheduleC => ("report.schedule-c", Read),
    ReportVendor1099 => ("report.1099", Read),
    ReportTaxSummary => ("report.tax-summary", Read),
    ReportTrend => ("report.trend", Read),
    ReportTrialBalance => ("report.trial-balance", Read),
    ReportTrialBalanceCompare => ("report.trial-balance.compare", Read),
    ReportTrialBalancePeriods => ("report.trial-balance.periods", Read),
    TxnPost => ("txn.post", Post),
    TxnImport => ("txn.import", Post),
    TxnAttach => ("txn.attach", Post),
    TxnMetaSet => ("txn.meta.set", Post),
    TxnMetaUnset => ("txn.meta.unset", Post),
    InvestmentBuy => ("investment.buy", Post),
    InvestmentSell => ("investment.sell", Post),
    SecurityCreate => ("security.create", Post),
    SecurityPrice => ("security.price", Post),
    LoanPay => ("loan.pay", Post),
    ScheduleCreate => ("schedule.create", Post),
    ScheduleCancel => ("schedule.cancel", Post),
    ScheduleRun => ("schedule.run", Post),
    RuleCreate => ("rule.create", Post),
    RuleDelete => ("rule.delete", Post),
    TxnPropose => ("txn.propose", Propose),
    TxnReject => ("txn.reject", ProposalReject),
    TxnApprove => ("txn.approve", Approve),
    TxnReverse => ("txn.reverse", Reverse),
    TxnClear => ("txn.clear", Reconcile),
    TxnReconcile => ("txn.reconcile", Reconcile),
    BudgetSet => ("budget.set", Budget),
    BudgetDelete => ("budget.delete", Budget),
    PeriodClose => ("period.close", Admin),
    PeriodReopen => ("period.reopen", Admin),
    CompanyCreate => ("company.create", Admin),
    CompanyDelete => ("company.delete", Admin),
    AccountCreate => ("account.create", Admin),
    AccountDelete => ("account.delete", Admin),
    KeyInit => ("key.init", Admin),
    AgentIssue => ("agent.issue", Admin),
    AgentRevoke => ("agent.revoke", Admin),
    PolicySet => ("policy.set", Admin),
    Init => ("init", Admin),
    Backup => ("backup", Admin),
    Restore => ("restore", Admin),
    AgentLimitSet => ("agent.limit.set", Admin),
    AgentLimitClear => ("agent.limit.clear", Admin),
    McpServe => ("mcp.serve", Transport),
}
