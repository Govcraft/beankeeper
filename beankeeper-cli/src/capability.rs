//! Shared CLI and MCP capability requirements. Unknown operations fail closed.

use crate::error::CliError;

/// Explicit sibling grants beneath the ledger identity path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Read,
    Post,
    Propose,
    Approve,
    Reverse,
    Reconcile,
    Budget,
    Admin,
}

impl Capability {
    /// The leaf path required by an operation.
    #[must_use]
    pub fn path(self) -> &'static str {
        match self {
            Self::Read => "ledger/read",
            Self::Propose => "ledger/propose",
            Self::Approve => "ledger/approve",
            Self::Post => "ledger/post",
            Self::Reverse => "ledger/reverse",
            Self::Reconcile => "ledger/reconcile",
            Self::Budget => "ledger/budget",
            Self::Admin => "ledger/admin",
        }
    }

    /// Classify a CLI command name or MCP tool name with the same policy.
    ///
    /// # Errors
    /// Returns a usage error for an operation without an explicit mapping.
    pub fn for_operation(operation: &str) -> Result<Self, CliError> {
        let operation = operation.replace('_', ".");
        match operation.as_str() {
            "period.list"
            | "company.list"
            | "company.show"
            | "account.list"
            | "account.show"
            | "txn.list"
            | "txn.show"
            | "security.list"
            | "investment.lots"
            | "investment.holdings"
            | "schedule.list"
            | "rule.list"
            | "budget.list"
            | "loan.schedule"
            | "export"
            | "verify"
            | "audit.log"
            | "txn.proposals"
            | "agent.limit.show"
            | "agent.list"
            | "policy.show"
            | "report"
            | "report.aging"
            | "report.balance"
            | "report.balance-sheet"
            | "report.budget-variance"
            | "report.cash-flow"
            | "report.income-statement"
            | "report.income-statement.compare"
            | "report.income-statement.periods"
            | "report.ledger"
            | "report.reconciliation"
            | "report.schedule-c"
            | "report.1099"
            | "report.1099-nec"
            | "report.tax-summary"
            | "report.trend"
            | "report.trial-balance"
            | "report.trial-balance.compare"
            | "report.trial-balance.periods" => Ok(Self::Read),
            "txn.post" | "txn.import" | "txn.attach" | "txn.meta.set" | "txn.meta.unset"
            | "investment.buy" | "investment.sell" | "security.create" | "security.price"
            | "loan.pay" | "schedule.create" | "schedule.cancel" | "schedule.run"
            | "rule.create" | "rule.delete" => Ok(Self::Post),
            "txn.propose" | "txn.reject" => Ok(Self::Propose),
            "txn.approve" => Ok(Self::Approve),
            "txn.reverse" => Ok(Self::Reverse),
            "txn.clear" | "txn.reconcile" => Ok(Self::Reconcile),
            "budget.set" | "budget.delete" => Ok(Self::Budget),
            "period.close" | "period.reopen" | "company.create" | "company.delete"
            | "account.create" | "account.delete" | "key.init" | "agent.issue" | "agent.revoke"
            | "policy.set" | "init" | "backup" | "restore" | "agent.limit.set"
            | "agent.limit.clear" => Ok(Self::Admin),
            _ => Err(CliError::Usage(
                "operation has no capability mapping".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn every_cli_operation_has_an_explicit_requirement() -> Result<(), CliError> {
        fn visit(command: &clap::Command, prefix: &str) -> Result<(), CliError> {
            for child in command.get_subcommands() {
                let name = if prefix.is_empty() {
                    child.get_name().to_owned()
                } else {
                    format!("{prefix}.{}", child.get_name())
                };
                if child.has_subcommands() {
                    visit(child, &name)?;
                } else if name != "mcp.serve" {
                    Capability::for_operation(&name)?;
                }
            }
            Ok(())
        }
        visit(&crate::cli::Cli::command(), "")?;
        assert!(Capability::for_operation("unknown.mutation").is_err());
        for (operation, expected) in [
            ("txn_post", Capability::Post),
            ("txn_meta_set", Capability::Post),
            ("txn.reverse", Capability::Reverse),
            ("txn_reconcile", Capability::Reconcile),
            ("account_create", Capability::Admin),
            ("backup", Capability::Admin),
            ("restore", Capability::Admin),
            ("report.schedule-c", Capability::Read),
            ("report.1099", Capability::Read),
            ("report.1099-nec", Capability::Read),
            ("report", Capability::Read),
        ] {
            assert_eq!(Capability::for_operation(operation)?, expected);
        }
        Ok(())
    }
}
