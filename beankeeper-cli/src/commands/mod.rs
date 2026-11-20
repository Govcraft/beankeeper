pub mod account;
pub mod agent;
pub mod audit;
pub mod backup;
pub mod budget;
pub mod company;
pub mod export;
pub mod import_ofx;
pub mod init;
pub mod investment;
pub mod loan;
pub mod policy;
pub mod report;
pub mod report_aging;
pub mod report_cash_flow;
pub mod report_currency;
mod report_dates;
pub mod report_ledger;
pub mod report_periods;
pub mod report_reconciliation;
pub mod report_tax;
pub mod report_trend;
pub mod rule;
pub mod schedule;
pub mod txn;
pub mod txn_clear;
pub mod txn_import;
pub mod txn_meta;
pub mod verify;

use crate::cli::{
    AccountCommand, AuditCommand, BudgetCommand, Cli, Command, CompanyCommand, ReportCommand,
    TxnCommand, require_company,
};
use crate::error::CliError;
use crate::output::json::Meta;

/// Map a parsed `Command` to its dot-notation command name.
#[must_use]
pub fn command_name(cmd: &Command) -> &'static str {
    if let Some(id) = report_periods::command_id(cmd) {
        return id;
    }
    match cmd {
        Command::Security(args) => args.command.command_name(),
        Command::Investment(args) => args.command.command_name(),
        Command::Schedule(args) => args.command.command_name(),
        Command::Loan(args) => match &args.command {
            crate::cli::LoanCommand::Schedule(_) => "loan.schedule",
            crate::cli::LoanCommand::Pay(_) => "loan.pay",
        },
        Command::Rule(args) => match &args.command {
            crate::cli::RuleCommand::Create { .. } => "rule.create",
            crate::cli::RuleCommand::List => "rule.list",
            crate::cli::RuleCommand::Delete { .. } => "rule.delete",
        },
        Command::Mcp(_) => "mcp.serve",
        Command::Policy(sub) => match sub {
            crate::cli::PolicyCommand::Show => "policy.show",
            crate::cli::PolicyCommand::Set { .. } => "policy.set",
        },
        Command::Key(_) => "key.init",
        Command::Agent(sub) => match sub {
            crate::cli::AgentCommand::Issue { .. } => "agent.issue",
            crate::cli::AgentCommand::List => "agent.list",
            crate::cli::AgentCommand::Revoke { .. } => "agent.revoke",
        },
        Command::Init { .. } => "init",
        Command::Backup { .. } => "backup",
        Command::Restore { .. } => "restore",
        Command::Verify => "verify",
        Command::Export { .. } => "export",
        Command::Company(sub) => match sub {
            CompanyCommand::Create { .. } => "company.create",
            CompanyCommand::List => "company.list",
            CompanyCommand::Show { .. } => "company.show",
            CompanyCommand::Delete { .. } => "company.delete",
        },
        Command::Account(args) => match &args.command {
            AccountCommand::Create { .. } => "account.create",
            AccountCommand::List { .. } => "account.list",
            AccountCommand::Show { .. } => "account.show",
            AccountCommand::Delete { .. } => "account.delete",
        },
        Command::Budget(args) => match &args.command {
            BudgetCommand::Set { .. } => "budget.set",
            BudgetCommand::List { .. } => "budget.list",
            BudgetCommand::Delete { .. } => "budget.delete",
        },
        Command::Txn(args) => match args.command.as_ref() {
            TxnCommand::Meta(sub) => sub.command_name(),
            TxnCommand::Post { .. } => "txn.post",
            TxnCommand::List { .. } => "txn.list",
            TxnCommand::Show { .. } => "txn.show",
            TxnCommand::Reverse { .. } => "txn.reverse",
            TxnCommand::Import { .. } => "txn.import",
            TxnCommand::Attach { .. } => "txn.attach",
            TxnCommand::Clear { .. } => "txn.clear",
            TxnCommand::Reconcile => "txn.reconcile",
        },
        Command::Report(args) => match &args.command {
            ReportCommand::TrialBalance { .. } => "report.trial-balance",
            ReportCommand::Balance { .. } => "report.balance",
            ReportCommand::IncomeStatement { .. } => "report.income-statement",
            ReportCommand::BalanceSheet { .. } => "report.balance-sheet",
            ReportCommand::TaxSummary { .. } => "report.tax-summary",
            ReportCommand::ScheduleC(_) => "report.schedule-c",
            ReportCommand::Vendor1099(_) => "report.1099",
            ReportCommand::BudgetVariance { .. } => "report.budget-variance",
            ReportCommand::Ledger { .. } => "report.ledger",
            ReportCommand::Aging { .. } => "report.aging",
            ReportCommand::Reconciliation { .. } => "report.reconciliation",
            ReportCommand::CashFlow(_) => "report.cash-flow",
            ReportCommand::Trend(_) => "report.trend",
        },
        Command::Audit(args) => match &args.command {
            AuditCommand::Log { .. } => "audit.log",
        },
    }
}

/// Dispatch a parsed CLI command.
///
/// # Errors
///
/// Returns [`CliError`] for any command-level failure.
pub fn dispatch(cli: &Cli, meta: Option<Meta>) -> Result<(), CliError> {
    if !matches!(cli.command, Command::Init { .. } | Command::Mcp(_) | Command::Restore { .. }) {
        // Preserve standalone loan calculation without an existing ledger.
        let standalone = matches!(&cli.command, Command::Loan(args) if matches!(args.command, crate::cli::LoanCommand::Schedule(_)))
            && !cli.db.try_exists()?
            && cli.agent_token.is_none();
        if !standalone {
            cli.open_db()?;
        }
    } else if cli.agent_token.is_some() && matches!(cli.command, Command::Init { .. }) {
        return Err(CliError::Unauthorized("initialization creates a new trust boundary; omit --agent-token and initialize its key afterward".into()));
    }
    let meta = meta.map(|m| cli.meta(&m.command, m.company.as_deref()));
    match &cli.command {
        Command::Backup { output, force } => backup::backup(cli, output, *force),
        Command::Restore { input, force } => backup::restore(cli, input, *force),
        Command::Security(_) | Command::Investment(_) => {
            let company = require_company(cli)?;
            investment::run(cli, &company)
        }
        Command::Schedule(args) => {
            let company = require_company(cli)?;
            schedule::run(cli, &company, &args.command)
        }
        Command::Loan(args) => loan::run(cli, &args.command),
        Command::Rule(args) => {
            let company = require_company(cli)?;
            rule::run(cli, &company, &args.command)
        }
        Command::Key(_) | Command::Agent(_) => agent::run(cli),
        Command::Policy(command) => policy::run(cli, command),
        Command::Mcp(crate::cli::McpCommand::Serve { read_only }) => {
            crate::mcp::serve(cli, *read_only)
        }
        Command::Init {
            encrypt,
            path,
            force,
            demo,
        } => init::run(cli, *encrypt, path.as_deref(), *force, *demo),

        Command::Verify => verify::run(cli),

        Command::Export { format, output, .. } => export::run(cli, *format, output.as_deref()),

        Command::Company(sub) => company::run(cli, sub),

        Command::Account(args) => {
            let company = require_company(cli)?;
            account::run(cli, &company, &args.command)
        }

        Command::Txn(args) => {
            // Reconcile scans all companies, so --company is not required.
            if matches!(args.command.as_ref(), crate::cli::TxnCommand::Reconcile) {
                txn::run(cli, "", args.command.as_ref(), meta)
            } else {
                let company = require_company(cli)?;
                txn::run(cli, &company, args.command.as_ref(), meta)
            }
        }

        Command::Budget(args) => {
            let company = require_company(cli)?;
            budget::run(cli, &company, &args.command)
        }

        Command::Report(args) => {
            let company = require_company(cli)?;
            report::run(cli, &company, &args.command)
        }

        // Audit is cross-company by design, so --company is not required.
        Command::Audit(args) => audit::run(cli, &args.command),
    }
}
