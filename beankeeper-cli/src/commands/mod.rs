pub mod account;
pub mod agent;
pub mod audit;
mod audit_snapshot;
pub mod backup;
pub mod bank_feed;
pub mod budget;
pub mod company;
pub mod counterparty;
pub mod document;
pub mod export;
pub mod fixed_asset;
pub mod fx;
pub mod import_ofx;
pub mod init;
pub mod investment;
pub mod loan;
mod payroll;
pub mod period;
pub mod policy;
mod posting_output;
pub mod reconcile;
pub mod reference;
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
mod setup_statement;
pub mod standing;
pub(crate) mod statement_source;
pub mod summary;
pub mod tax;
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
        Command::BankFeed(command) => command.operation(),
        Command::Setup(command) => command.operation(),
        Command::Security(args) => args.command.operation(),
        Command::Investment(args) => args.command.operation(),
        Command::Schedule(args) => args.command.operation(),
        Command::Loan(args) => match &args.command {
            crate::cli::LoanCommand::Schedule(_) => Operation::LoanSchedule,
            crate::cli::LoanCommand::Pay(_) => Operation::LoanPay,
        },
        Command::Payroll(crate::cli::PayrollCommand::Preview { .. }) => Operation::PayrollPreview,
        Command::Payroll(crate::cli::PayrollCommand::Import { .. }) => Operation::PayrollImport,
        Command::Payroll(crate::cli::PayrollCommand::Fund { .. }) => Operation::PayrollFund,
        Command::Payroll(crate::cli::PayrollCommand::Show { .. }) => Operation::PayrollShow,
        Command::Rule(args) => args.command.operation(),
        Command::Counterparty(args) => args.command.operation(),
        Command::Document(args) => args.command.operation(),
        Command::Tax(command) => command.operation(),
        Command::BookkeepingPolicy(command) => command.operation(),
        Command::InformPreference(command) => command.operation(),
        Command::Period(command) => period::operation(command),
        Command::Reconcile(command) => command.operation(),
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
        Command::RestoreUndo { .. } => Operation::RestoreUndo,
        Command::Recover { .. } => Operation::Recover,
        Command::Verify => Operation::Verify,
        Command::Export { .. } => Operation::Export,
        Command::Company(sub) => match sub {
            CompanyCommand::Create { .. } => Operation::CompanyCreate,
            CompanyCommand::List => Operation::CompanyList,
            CompanyCommand::Show { .. } => Operation::CompanyShow,
            CompanyCommand::Delete { .. } => Operation::CompanyDelete,
        },
        Command::Fx(args) => args.command.operation(),
        Command::FixedAsset(args) => args.command.operation(),
        Command::Account(args) => match &args.command {
            AccountCommand::Create { .. } => Operation::AccountCreate,
            AccountCommand::Rename { .. } => Operation::AccountRename,
            AccountCommand::Classify { .. } => Operation::AccountClassify,
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
            TxnCommand::Link { .. } => Operation::TxnLink,
            TxnCommand::Reverse(_) => Operation::TxnReverse,
            TxnCommand::Recategorize(_) => Operation::TxnRecategorize,
            TxnCommand::SettleHold(_) => Operation::TxnSettleHold,
            TxnCommand::Import(_) => Operation::TxnImport,
            TxnCommand::Attach(_) => Operation::TxnAttach,
            TxnCommand::Clear(_) => Operation::TxnClear,
            TxnCommand::Reconcile => Operation::TxnReconcile,
        },
        Command::Report(args) => report_operation(&args.command),
        Command::Audit(args) => match &args.command {
            AuditCommand::Log(_) => Operation::AuditLog,
            AuditCommand::Receipts(_) => Operation::AuditReceipts,
            AuditCommand::Undo(_) => Operation::AuditUndo,
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
        ReportCommand::BudgetVariance(_) => Operation::ReportBudgetVariance,
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
    dispatch_command(cli, meta)?;
    if !cli.verbosity.quiet
        && can_append_audit(&cli.command)
        && let Some(notice) = cli.principal().unverified_hint_notice()
    {
        audit::show_hint_notice(&notice);
    }
    Ok(())
}

/// Dispatch a parsed ledger command against a connection opened by the host.
///
/// The supplied ledger determines the command's target. Authorization and
/// output still follow the CLI options. File creation, recovery, and MCP
/// startup have their own entry points.
///
/// # Errors
/// Returns [`CliError`] for an unsupported command or command-level failure.
pub fn dispatch_with_db(
    cli: &mut Cli,
    db: crate::db::Db,
    meta: Option<Meta>,
) -> Result<(), CliError> {
    if matches!(
        cli.command,
        Command::Init { .. } | Command::Recover { .. } | Command::Mcp(_)
    ) {
        return Err(CliError::Usage(
            "this command has a separate file or transport entry point".into(),
        ));
    }
    cli.with_prepared_ledger(db, |cli| dispatch(cli, meta))
}

fn can_append_audit(command: &Command) -> bool {
    match command {
        Command::Init { demo, .. } => *demo,
        Command::Txn(args) if matches!(args.command.as_ref(), TxnCommand::Import(args) if args.dry_run) => {
            false
        }
        Command::Schedule(args)
            if matches!(
                args.command,
                crate::cli::ScheduleCommand::Run { dry_run: true, .. }
            ) =>
        {
            false
        }
        _ => operation(command).writes_ledger(),
    }
}

fn dispatch_command(cli: &Cli, meta: Option<Meta>) -> Result<(), CliError> {
    if matches!(cli.command, Command::BankFeed(_)) {
        bank_feed::check_output(cli)?;
    }
    let chart = report::ChartRequest::try_from((cli.output.chart, cli.output.chart_all))?;
    chart.validate_cli(cli)?;
    if dispatch_ledger(cli, meta.clone(), chart)? {
        return Ok(());
    }
    dispatch_local(cli, meta)
}

fn dispatch_ledger(
    cli: &Cli,
    meta: Option<Meta>,
    chart: report::ChartRequest,
) -> Result<bool, CliError> {
    use crate::request::LedgerRequest;

    let request = match &cli.command {
        Command::Setup(command) => return dispatch_setup(cli, meta, command).map(|()| true),
        Command::BankFeed(command) => bank_feed::ledger_request(cli, command)?,
        Command::Summary(args) => Some(LedgerRequest::Summary(
            (require_company(cli)?, args, chart).try_into()?,
        )),
        Command::Period(command) => Some(LedgerRequest::Period(
            (require_company(cli)?.clone(), command).try_into()?,
        )),
        Command::Reconcile(command) => Some(LedgerRequest::Reconcile(
            (require_company(cli)?.clone(), command).try_into()?,
        )),
        Command::Security(args) => Some(LedgerRequest::Investment(
            (require_company(cli)?.clone(), &args.command).try_into()?,
        )),
        Command::Investment(args) => Some(LedgerRequest::Investment(
            (require_company(cli)?.clone(), &args.command).try_into()?,
        )),
        Command::Verify => Some(LedgerRequest::Verify),
        Command::Company(command) => {
            return execute_confirmable(cli, meta, "Aborted.", || {
                let request: Option<company::CompanyRequest> = command.try_into()?;
                Ok(request.map(LedgerRequest::Company))
            });
        }
        Command::FixedAsset(args) => Some(LedgerRequest::FixedAsset(
            (require_company(cli)?, &args.command).try_into()?,
        )),
        Command::Fx(args) => Some(LedgerRequest::Fx(
            (require_company(cli)?, &args.command).into(),
        )),
        Command::Account(args) => {
            return execute_confirmable(cli, meta, "Aborted.", || {
                let request: Option<account::AccountRequest> =
                    account::AccountCliInput(require_company(cli)?, &args.command).try_into()?;
                Ok(request.map(LedgerRequest::Account))
            });
        }
        Command::Txn(args) => match args.command.as_ref() {
            TxnCommand::Import(_) | TxnCommand::Attach(_) => None,
            command => {
                let company = if matches!(command, TxnCommand::Reconcile) {
                    None
                } else {
                    Some(require_company(cli)?)
                };
                Some(LedgerRequest::Transaction(txn::TxnRequest::try_from_cli(
                    company, command,
                )?))
            }
        },
        Command::Rule(args) => {
            return execute_confirmable(cli, meta, "aborted", || {
                let request: Option<rule::RuleRequest> =
                    rule::RuleCliInput(require_company(cli)?, &args.command).try_into()?;
                Ok(request.map(LedgerRequest::Rule))
            });
        }
        Command::Counterparty(args) => Some(LedgerRequest::Counterparty(
            (require_company(cli)?.clone(), args.command.clone()).into(),
        )),
        Command::Tax(command) => Some(tax::ledger_request(cli, command)?),
        Command::Document(args) => Some(LedgerRequest::Document(
            (require_company(cli)?.clone(), args.command.clone()).into(),
        )),
        Command::BookkeepingPolicy(command) => Some(LedgerRequest::Standing(
            (require_company(cli)?, command).into(),
        )),
        Command::InformPreference(command) => Some(LedgerRequest::Standing(
            (require_company(cli)?, command).into(),
        )),
        Command::Budget(args) => {
            return execute_confirmable(cli, meta, "aborted", || {
                let request: Option<budget::BudgetRequest> =
                    budget::BudgetCliInput(require_company(cli)?, &args.command).try_into()?;
                Ok(request.map(LedgerRequest::Budget))
            });
        }
        Command::Report(args) => Some(LedgerRequest::Report(
            (require_company(cli)?, &args.command, chart).try_into()?,
        )),
        Command::Audit(args) => match &args.command {
            AuditCommand::Log(args) => Some(LedgerRequest::AuditLog(args.into())),
            AuditCommand::Receipts(args) => Some(LedgerRequest::AuditReceipts(
                audit::ReceiptRequest::List((require_company(cli)?, args).try_into()?),
            )),
            AuditCommand::Undo(args) => Some(LedgerRequest::AuditReceipts(
                audit::ReceiptRequest::Undo(require_company(cli)?.clone(), args.receipt),
            )),
        },
        _ => None,
    };
    if let Some(request) = request {
        let db = cli.command_db()?;
        let meta = meta.map(|meta| cli.meta(meta.command, meta.company.as_deref()));
        request.execute(&db, &cli.principal())?.render(cli, meta)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn dispatch_setup(
    cli: &Cli,
    meta: Option<Meta>,
    command: &setup::SetupCommand,
) -> Result<(), CliError> {
    use crate::request::LedgerRequest;

    let request = setup::SetupRequest::try_from(command)?;
    let response = if request.reads_compatible_snapshot() {
        let snapshot;
        let db = if let Some(db) = cli.prepared_ledger() {
            db
        } else {
            snapshot = setup::open_setup_snapshot(cli)?;
            &snapshot
        };
        LedgerRequest::Setup(request)
            .execute_with_actor(db, |_| {
                cli.verify_agent(db)?;
                Ok(cli.principal())
            })?
            .0
    } else {
        let db = cli.command_db()?;
        LedgerRequest::Setup(request).execute(&db, &cli.principal())?
    };
    response.render(cli, meta)
}

fn execute_confirmable(
    cli: &Cli,
    meta: Option<Meta>,
    aborted: &str,
    prepare: impl FnOnce() -> Result<Option<crate::request::LedgerRequest>, CliError>,
) -> Result<bool, CliError> {
    // Establish the ledger and caller before asking for confirmation.
    let db = cli.command_db()?;
    if let Some(request) = prepare()? {
        request.execute(&db, &cli.principal())?.render(cli, meta)?;
    } else {
        eprintln!("{aborted}");
    }
    Ok(true)
}

fn dispatch_local(cli: &Cli, meta: Option<Meta>) -> Result<(), CliError> {
    if !matches!(
        cli.command,
        Command::Init { .. }
            | Command::Mcp(_)
            | Command::Backup { .. }
            | Command::Restore { .. }
            | Command::RestoreUndo { .. }
            | Command::Recover { .. }
            | Command::Setup(_)
    ) {
        // Preserve standalone loan calculation without an existing ledger.
        let standalone = matches!(&cli.command, Command::Loan(args) if matches!(args.command, crate::cli::LoanCommand::Schedule(_)))
            && cli.prepared_ledger().is_none()
            && !cli.db_path()?.try_exists()?
            && cli.agent_token.is_none();
        if !standalone {
            cli.command_db()?;
        }
    } else if cli.agent_token.is_some() && matches!(cli.command, Command::Init { .. }) {
        return Err(CliError::Unauthorized("initialization creates a new trust boundary; omit --agent-token and initialize its key afterward".into()));
    }
    let meta = meta.map(|m| cli.meta(m.command, m.company.as_deref()));
    match &cli.command {
        Command::BankFeed(command) => bank_feed::run(cli, command),
        Command::Backup { output, overwrite } => backup::backup(cli, output, *overwrite),
        Command::Restore { input } => backup::restore(
            cli,
            crate::db::audit::restore::RestoreRequest::Backup(input.clone()),
        ),
        Command::RestoreUndo { undo_id } => backup::restore(
            cli,
            crate::db::audit::restore::RestoreRequest::Undo(*undo_id),
        ),
        Command::Recover { input } => backup::recover(cli, input),
        Command::Schedule(args) => {
            let company = require_company(cli)?.as_str();
            schedule::run(cli, company, &args.command)
        }
        Command::Loan(args) => loan::run(cli, &args.command),
        Command::Payroll(command) => payroll::run(cli, command),
        Command::Key(crate::cli::KeyCommand::Init) => agent::run_key(cli),
        Command::Agent(command) => agent::run(cli, command),
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
            profile,
        } => init::run_selected(
            cli,
            *encrypt,
            path.as_deref(),
            (*demo, *force, profile.as_deref()).try_into()?,
        ),

        Command::Export { format, output, .. } => export::run(cli, *format, output.as_deref()),

        Command::Txn(args) => {
            txn::run_for_company(cli, require_company(cli)?, args.command.as_ref(), meta)
        }
        Command::Setup(_)
        | Command::Summary(_)
        | Command::Period(_)
        | Command::Reconcile(_)
        | Command::Security(_)
        | Command::Investment(_)
        | Command::Verify
        | Command::Company(_)
        | Command::Account(_)
        | Command::FixedAsset(_)
        | Command::Fx(_)
        | Command::Budget(_)
        | Command::Rule(_)
        | Command::Counterparty(_)
        | Command::Document(_)
        | Command::Tax(_)
        | Command::BookkeepingPolicy(_)
        | Command::InformPreference(_)
        | Command::Report(_)
        | Command::Audit(_) => Err(CliError::General(
            "ledger request was not dispatched".into(),
        )),
    }
}
