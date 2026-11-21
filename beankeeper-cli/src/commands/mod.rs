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
pub mod period;
pub mod policy;
pub(crate) mod reference;
pub mod report;
pub mod report_aging;
pub mod report_cash_flow;
pub(crate) mod report_compare;
pub mod report_currency;
pub(crate) mod report_dates;
pub mod report_ledger;
pub mod report_periods;
pub mod report_reconciliation;
pub mod report_tax;
pub mod report_trend;
pub mod rule;
pub mod schedule;
pub mod setup;
pub mod summary;
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
use crate::operation::Operation;
use crate::output::json::Meta;

/// Return the canonical name of a parsed command.
#[must_use]
pub fn command_name(cmd: &Command) -> &'static str {
    operation(cmd).name()
}

/// Return the authorization operation represented by a parsed command.
#[must_use]
pub fn operation(cmd: &Command) -> Operation {
    match cmd {
        Command::Summary(_) => Operation::Summary,
        Command::Setup(command) => command.operation(),
        Command::Security(args) => args.command.operation(),
        Command::Investment(args) => args.command.operation(),
        Command::Schedule(args) => args.command.operation(),
        Command::Loan(args) => match &args.command {
            crate::cli::LoanCommand::Schedule(_) => Operation::LoanSchedule,
            crate::cli::LoanCommand::Pay(_) => Operation::LoanPay,
        },
        Command::Rule(args) => match &args.command {
            crate::cli::RuleCommand::Create { .. } => Operation::RuleCreate,
            crate::cli::RuleCommand::List => Operation::RuleList,
            crate::cli::RuleCommand::Delete { .. } => Operation::RuleDelete,
        },
        Command::Period(command) => period::operation(command),
        Command::Mcp(_) => Operation::McpServe,
        Command::Policy(sub) => match sub {
            crate::cli::PolicyCommand::Show => Operation::PolicyShow,
            crate::cli::PolicyCommand::Set { .. } => Operation::PolicySet,
        },
        Command::Key(_) => Operation::KeyInit,
        Command::Agent(sub) => match sub {
            crate::cli::AgentCommand::Issue { .. } => Operation::AgentIssue,
            crate::cli::AgentCommand::Limit(sub) => match sub {
                crate::cli::AgentLimitCommand::Set { .. } => Operation::AgentLimitSet,
                crate::cli::AgentLimitCommand::Show { .. } => Operation::AgentLimitShow,
                crate::cli::AgentLimitCommand::Clear { .. } => Operation::AgentLimitClear,
            },
            crate::cli::AgentCommand::List => Operation::AgentList,
            crate::cli::AgentCommand::Revoke { .. } => Operation::AgentRevoke,
        },
        Command::Init { .. } => Operation::Init,
        Command::Backup { .. } => Operation::Backup,
        Command::Restore { .. } => Operation::Restore,
        Command::Verify => Operation::Verify,
        Command::Export { .. } => Operation::Export,
        Command::Company(sub) => match sub {
            CompanyCommand::Create { .. } => Operation::CompanyCreate,
            CompanyCommand::List => Operation::CompanyList,
            CompanyCommand::Show { .. } => Operation::CompanyShow,
            CompanyCommand::Delete { .. } => Operation::CompanyDelete,
        },
        Command::Account(args) => match &args.command {
            AccountCommand::Create { .. } => Operation::AccountCreate,
            AccountCommand::List { .. } => Operation::AccountList,
            AccountCommand::Show { .. } => Operation::AccountShow,
            AccountCommand::Delete { .. } => Operation::AccountDelete,
        },
        Command::Budget(args) => match &args.command {
            BudgetCommand::Set(_) => Operation::BudgetSet,
            BudgetCommand::List { .. } => Operation::BudgetList,
            BudgetCommand::Delete { .. } => Operation::BudgetDelete,
        },
        Command::Txn(args) => match args.command.as_ref() {
            TxnCommand::Meta(sub) => sub.operation(),
            TxnCommand::Post(_) => Operation::TxnPost,
            TxnCommand::Proposal(command) => match command {
                crate::cli::TxnProposalCommand::Propose(_) => Operation::TxnPropose,
                crate::cli::TxnProposalCommand::Proposals { .. } => Operation::TxnProposals,
                crate::cli::TxnProposalCommand::Approve { .. } => Operation::TxnApprove,
                crate::cli::TxnProposalCommand::Reject { .. } => Operation::TxnReject,
            },
            TxnCommand::List(_) => Operation::TxnList,
            TxnCommand::Show { .. } => Operation::TxnShow,
            TxnCommand::Reverse(_) => Operation::TxnReverse,
            TxnCommand::Import(_) => Operation::TxnImport,
            TxnCommand::Attach(_) => Operation::TxnAttach,
            TxnCommand::Clear(_) => Operation::TxnClear,
            TxnCommand::Reconcile => Operation::TxnReconcile,
        },
        Command::Report(args) => report_operation(&args.command),
        Command::Audit(args) => match &args.command {
            AuditCommand::Log { .. } => Operation::AuditLog,
        },
    }
}

fn report_operation(command: &ReportCommand) -> Operation {
    match command {
        ReportCommand::TrialBalance {
            period: Some(_), ..
        } => Operation::ReportTrialBalancePeriods,
        ReportCommand::TrialBalance {
            compare: Some(_), ..
        } => Operation::ReportTrialBalanceCompare,
        ReportCommand::TrialBalance { .. } => Operation::ReportTrialBalance,
        ReportCommand::Balance { .. } => Operation::ReportBalance,
        ReportCommand::IncomeStatement {
            period: Some(_), ..
        } => Operation::ReportIncomeStatementPeriods,
        ReportCommand::IncomeStatement {
            compare: Some(_), ..
        } => Operation::ReportIncomeStatementCompare,
        ReportCommand::IncomeStatement { .. } => Operation::ReportIncomeStatement,
        ReportCommand::BalanceSheet { .. } => Operation::ReportBalanceSheet,
        ReportCommand::TaxSummary { .. } => Operation::ReportTaxSummary,
        ReportCommand::ScheduleC(_) => Operation::ReportScheduleC,
        ReportCommand::Vendor1099(_) => Operation::ReportVendor1099,
        ReportCommand::BudgetVariance { .. } => Operation::ReportBudgetVariance,
        ReportCommand::Ledger { .. } => Operation::ReportLedger,
        ReportCommand::Aging(_) => Operation::ReportAging,
        ReportCommand::Reconciliation(_) => Operation::ReportReconciliation,
        ReportCommand::CashFlow(_) => Operation::ReportCashFlow,
        ReportCommand::Trend(_) => Operation::ReportTrend,
    }
}

/// Dispatch a parsed CLI command.
///
/// # Errors
///
/// Returns [`CliError`] for any command-level failure.
pub fn dispatch(cli: &Cli, meta: Option<Meta>) -> Result<(), CliError> {
    crate::output::chart::validate_request(cli)?;
    if !matches!(
        cli.command,
        Command::Init { .. } | Command::Mcp(_) | Command::Restore { .. } | Command::Setup(_)
    ) {
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
        Command::Setup(command) => setup::run(cli, command),
        Command::Summary(args) => {
            let company = require_company(cli)?;
            summary::run(cli, &company, args)
        }
        Command::Period(command) => {
            let company = require_company(cli)?;
            period::run(cli, &company, command)
        }
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
        Command::Mcp(crate::cli::McpCommand::Serve {
            read_only,
            http,
            bind,
            allow_remote,
        }) => match http {
            true => crate::mcp::serve_http(
                cli,
                *read_only,
                bind.unwrap_or_else(|| ([127, 0, 0, 1], 8750).into()),
                *allow_remote,
            ),
            false => crate::mcp::serve(cli, *read_only),
        },
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
                txn::run_reconcile_all(cli, crate::db::companies::AllCompanies)
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
