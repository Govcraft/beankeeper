//! Shared CLI and MCP capability requirements. Unknown operations fail closed.

use crate::error::CliError;

/// Explicit grants beneath the ledger or UI identity path.
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
    /// Scoped authority for an active ledger setup session.
    Setup,
    /// Host navigation authority, issued to a separate UI-scoped identity.
    Navigate,
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
            Self::Setup => "ledger/setup",
            Self::Navigate => "ui/navigate",
        }
    }

    /// Classify a CLI command name or MCP tool name with the same policy.
    ///
    /// # Errors
    /// Returns a usage error for an operation without an explicit mapping.
    pub fn for_operation(operation: &str) -> Result<Self, CliError> {
        operation
            .parse::<crate::operation::Operation>()?
            .permission()
            .primary()
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
