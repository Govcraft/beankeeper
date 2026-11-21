//! Translate decoded MCP inputs to the shared command entry points.

use beankeeper::core::LotMethod;
use beankeeper::types::Currency;
use serde_json::Value;

use super::input_values::{Month, PageLimit, PageOffset, Text};
use super::inputs;
use crate::cli::{
    self, AccountCommand, AuditCommand, BudgetCommand, Command, CompanyCommand, InvestmentCommand,
    PeriodCommand, SecurityCommand, TradeArgs, TxnCommand, TxnProposalCommand,
};
use crate::commands;
use crate::commands::txn::{PostingMode, PostingRequest};
use crate::db::{Actor, Db, audit::MetadataTarget, metadata};
use crate::error::CliError;
use crate::operation::Operation;

type Response = Result<(Value, Option<Value>), CliError>;

macro_rules! requests {
    ($($variant:ident($input:ident)),+ $(,)?) => {
        pub(super) enum Request {
            $($variant(inputs::$input)),+
        }

        impl Request {
            pub(super) const fn operation(&self) -> Operation {
                match self {
                    $(Self::$variant(_) => Operation::$variant),+
                }
            }
        }
    };
}

requests! {
    Summary(FinancialSummary),
    SecurityCreate(SecurityCreate),
    SecurityList(CompanyScope),
    SecurityPrice(SecurityPrice),
    InvestmentBuy(InvestmentBuy),
    InvestmentSell(InvestmentSell),
    InvestmentLots(InvestmentLots),
    InvestmentHoldings(InvestmentHoldings),
    CompanyList(Empty),
    CompanyShow(CompanyShow),
    AccountList(AccountList),
    AccountShow(AccountShow),
    AccountCreate(AccountCreate),
    TxnPost(TxnPost),
    TxnPropose(TxnPost),
    TxnProposals(TxnProposals),
    TxnApprove(TxnApprove),
    TxnReject(TxnReject),
    TxnReverse(TxnReverse),
    TxnMetaSet(TxnMetaSet),
    TxnMetaUnset(TxnMetaUnset),
    TxnList(TxnList),
    TxnShow(TxnShow),
    Report(Report),
    BudgetList(BudgetList),
    BudgetSet(BudgetSet),
    BudgetDelete(BudgetDelete),
    TxnClear(ClearInput),
    TxnReconcile(Empty),
    PeriodClose(PeriodClose),
    PeriodReopen(PeriodReopen),
    PeriodList(CompanyScope),
    Verify(Empty),
    AuditLog(AuditLog),
}

impl Request {
    pub(super) fn execute(self, db: &Db, actor: &Actor) -> Response {
        match self {
            Self::Summary(input) => summary(db, input, actor),
            Self::Report(input) => super::report_input::execute(db, input),
            Self::SecurityCreate(input) => call_security_create(db, actor, input),
            Self::SecurityList(input) => call_security_list(db, actor, input),
            Self::SecurityPrice(input) => call_security_price(db, actor, input),
            Self::InvestmentBuy(input) => call_investment_buy(db, actor, input),
            Self::InvestmentSell(input) => call_investment_sell(db, actor, input),
            Self::InvestmentLots(input) => call_investment_lots(db, actor, input),
            Self::InvestmentHoldings(input) => call_investment_holdings(db, actor, input),
            Self::CompanyList(_) => call_company_list(db),
            Self::CompanyShow(input) => call_company_show(db, &input),
            Self::AccountList(input) => call_account_list(db, actor, input),
            Self::AccountShow(input) => call_account_show(db, actor, input),
            Self::AccountCreate(input) => call_account_create(db, actor, input),
            Self::TxnPost(input) => call_txn_post(db, actor, input),
            Self::TxnPropose(input) => call_txn_propose(db, actor, input),
            Self::TxnProposals(input) => call_txn_proposals(db, actor, input),
            Self::TxnApprove(input) => call_txn_approve(db, actor, input),
            Self::TxnReject(input) => call_txn_reject(db, actor, input),
            Self::TxnReverse(input) => call_txn_reverse(db, actor, input),
            Self::TxnMetaSet(input) => call_txn_meta_set(db, actor, input),
            Self::TxnMetaUnset(input) => call_txn_meta_unset(db, actor, input),
            Self::TxnList(input) => call_txn_list(db, actor, input),
            Self::TxnShow(input) => call_txn_show(db, actor, input),
            Self::BudgetList(input) => call_budget_list(db, actor, input),
            Self::BudgetSet(input) => call_budget_set(db, actor, input),
            Self::BudgetDelete(input) => call_budget_delete(db, actor, input),
            Self::TxnClear(input) => clear(db, actor, input),
            Self::TxnReconcile(_) => call_txn_reconcile(db),
            Self::PeriodClose(input) => call_period_close(db, actor, input),
            Self::PeriodReopen(input) => call_period_reopen(db, actor, input),
            Self::PeriodList(input) => call_period_list(db, actor, input),
            Self::Verify(_) => call_verify(db),
            Self::AuditLog(input) => audit_log(db, input),
        }
    }
}

fn call_security_create(db: &Db, actor: &Actor, input: inputs::SecurityCreate) -> Response {
    execute_security(
        db,
        actor,
        input.company.into_inner().as_str(),
        &SecurityCommand::Create {
            symbol: input.symbol.to_string(),
            name: input.name,
            currency: input.currency.map_or(Currency::USD, Text::into_inner),
        },
    )
}

fn call_security_list(db: &Db, actor: &Actor, input: inputs::CompanyScope) -> Response {
    execute_security(
        db,
        actor,
        input.company.into_inner().as_str(),
        &SecurityCommand::List,
    )
}

fn call_security_price(db: &Db, actor: &Actor, input: inputs::SecurityPrice) -> Response {
    execute_security(
        db,
        actor,
        input.company.into_inner().as_str(),
        &SecurityCommand::Price {
            symbol: input.symbol.to_string(),
            date: input.date.into_inner().date(),
            price: input.price.to_string(),
        },
    )
}

fn call_investment_buy(db: &Db, actor: &Actor, input: inputs::InvestmentBuy) -> Response {
    execute_investment(
        db,
        actor,
        input.company.into_inner().as_str(),
        &InvestmentCommand::Buy {
            trade: TradeArgs {
                symbol: input.symbol.to_string(),
                quantity: input.quantity.into_inner(),
                date: input.date.into_inner().date(),
                asset: input.asset.to_string(),
                cash: input.cash.to_string(),
                reference: optional_text(input.reference),
            },
            cost: input.cost.to_string(),
        },
    )
}

fn call_investment_sell(db: &Db, actor: &Actor, input: inputs::InvestmentSell) -> Response {
    execute_investment(
        db,
        actor,
        input.company.into_inner().as_str(),
        &InvestmentCommand::Sell {
            trade: TradeArgs {
                symbol: input.symbol.to_string(),
                quantity: input.quantity.into_inner(),
                date: input.date.into_inner().date(),
                asset: input.asset.to_string(),
                cash: input.cash.to_string(),
                reference: optional_text(input.reference),
            },
            proceeds: input.proceeds.to_string(),
            gain: input.gain.to_string(),
            method: match input.method {
                None | Some(inputs::LotMethodArg::Fifo) => LotMethod::Fifo,
                Some(inputs::LotMethodArg::Lifo) => LotMethod::Lifo,
                Some(inputs::LotMethodArg::Specific) => LotMethod::Specific,
            },
            lot: input.lot.map(Text::into_inner),
        },
    )
}

fn call_investment_lots(db: &Db, actor: &Actor, input: inputs::InvestmentLots) -> Response {
    execute_investment(
        db,
        actor,
        input.company.into_inner().as_str(),
        &InvestmentCommand::Lots {
            symbol: input.symbol.to_string(),
        },
    )
}

fn call_investment_holdings(db: &Db, actor: &Actor, input: inputs::InvestmentHoldings) -> Response {
    execute_investment(
        db,
        actor,
        input.company.into_inner().as_str(),
        &InvestmentCommand::Holdings {
            as_of: input.as_of.map(|date| date.into_inner().date()),
        },
    )
}

fn call_company_list(db: &Db) -> Response {
    unpaged(commands::company::execute_json(db, &CompanyCommand::List))
}

fn call_company_show(db: &Db, input: &inputs::CompanyShow) -> Response {
    unpaged(commands::company::execute_json(
        db,
        &CompanyCommand::Show {
            slug: input.slug.to_string(),
        },
    ))
}

fn call_account_list(db: &Db, actor: &Actor, input: inputs::AccountList) -> Response {
    execute_account(
        db,
        actor,
        input.company.into_inner().as_str(),
        &AccountCommand::List {
            account_type: input.account_type.map(Into::into),
            name: input.name,
            with_balances: input.with_balances,
            from: optional_text(input.from),
            to: optional_text(input.to),
        },
    )
}

fn call_account_show(db: &Db, actor: &Actor, input: inputs::AccountShow) -> Response {
    execute_account(
        db,
        actor,
        input.company.into_inner().as_str(),
        &AccountCommand::Show {
            code: input.code.to_string(),
        },
    )
}

fn call_account_create(db: &Db, actor: &Actor, input: inputs::AccountCreate) -> Response {
    execute_account(
        db,
        actor,
        input.company.into_inner().as_str(),
        &AccountCommand::Create {
            code: input.code.to_string(),
            name: input.name,
            account_type: input.account_type.into(),
            default_tax_category: input.default_tax_category,
        },
    )
}

fn call_txn_post(db: &Db, actor: &Actor, input: inputs::TxnPost) -> Response {
    posting(db, input, actor, PostingMode::Post)
}

fn call_txn_propose(db: &Db, actor: &Actor, input: inputs::TxnPost) -> Response {
    posting(db, input, actor, PostingMode::Propose)
}

fn call_txn_proposals(db: &Db, actor: &Actor, input: inputs::TxnProposals) -> Response {
    execute_txn(
        db,
        actor,
        input.company.into_inner().as_str(),
        TxnCommand::Proposal(TxnProposalCommand::Proposals {
            include_rejected: input.include_rejected,
        }),
    )
}

fn call_txn_approve(db: &Db, actor: &Actor, input: inputs::TxnApprove) -> Response {
    execute_txn(
        db,
        actor,
        input.company.into_inner().as_str(),
        TxnCommand::Proposal(TxnProposalCommand::Approve {
            id: input.id.to_string(),
        }),
    )
}

fn call_txn_reject(db: &Db, actor: &Actor, input: inputs::TxnReject) -> Response {
    execute_txn(
        db,
        actor,
        input.company.into_inner().as_str(),
        TxnCommand::Proposal(TxnProposalCommand::Reject {
            id: input.id.to_string(),
            reason: input.reason,
        }),
    )
}

fn call_txn_reverse(db: &Db, actor: &Actor, input: inputs::TxnReverse) -> Response {
    execute_txn(
        db,
        actor,
        input.company.into_inner().as_str(),
        TxnCommand::Reverse(cli::TxnReverseArgs {
            id: input.id.get(),
            date: optional_text(input.date),
            description: input.description,
            reference: optional_text(input.reference),
        }),
    )
}

fn call_txn_meta_set(db: &Db, actor: &Actor, input: inputs::TxnMetaSet) -> Response {
    let run = |company: &str, target, values| {
        let edit = metadata::MetadataEdit::Set(metadata::parse_meta_args(values)?);
        unpaged(commands::txn_meta::execute_structured(
            db, company, actor, target, &edit,
        ))
    };
    match input {
        inputs::TxnMetaSet::Id {
            company,
            id,
            values,
        } => run(
            company.into_inner().as_str(),
            MetadataTarget::Id(id.get()),
            &values.into_vec(),
        ),
        inputs::TxnMetaSet::Reference {
            company,
            reference,
            values,
        } => run(
            company.into_inner().as_str(),
            MetadataTarget::Reference(reference.into_inner().as_str()),
            &values.into_vec(),
        ),
    }
}

fn call_txn_meta_unset(db: &Db, actor: &Actor, input: inputs::TxnMetaUnset) -> Response {
    let run = |company: &str, target, keys| {
        let edit = metadata::MetadataEdit::Unset(keys);
        unpaged(commands::txn_meta::execute_structured(
            db, company, actor, target, &edit,
        ))
    };
    match input {
        inputs::TxnMetaUnset::Id { company, id, keys } => run(
            company.into_inner().as_str(),
            MetadataTarget::Id(id.get()),
            keys.into_vec(),
        ),
        inputs::TxnMetaUnset::Reference {
            company,
            reference,
            keys,
        } => run(
            company.into_inner().as_str(),
            MetadataTarget::Reference(reference.into_inner().as_str()),
            keys.into_vec(),
        ),
    }
}

fn call_txn_list(db: &Db, actor: &Actor, input: inputs::TxnList) -> Response {
    execute_txn(
        db,
        actor,
        input.company.into_inner().as_str(),
        TxnCommand::List(cli::TxnListArgs {
            account: optional_text(input.account),
            from: optional_text(input.from),
            to: optional_text(input.to),
            limit: list_limit(input.limit.as_ref())?,
            offset: u32::try_from(input.offset.as_ref().map_or(0, PageOffset::get))
                .map_err(|_| CliError::Usage("offset must be a nonnegative row count".into()))?,
            description: input.description,
            amount_gt: optional_text(input.amount_gt),
            amount_lt: optional_text(input.amount_lt),
            amount_eq: optional_text(input.amount_eq),
            currency: optional_text(input.currency),
            reference: optional_text(input.reference),
            metadata: input.metadata,
            meta: input.meta,
            tax_category: input.tax_category,
            direction: input.direction.map(Into::into),
            count: input.count,
        }),
    )
}

fn call_txn_show(db: &Db, actor: &Actor, input: inputs::TxnShow) -> Response {
    execute_txn(
        db,
        actor,
        input.company.into_inner().as_str(),
        TxnCommand::Show { id: input.id.get() },
    )
}

fn call_budget_list(db: &Db, actor: &Actor, input: inputs::BudgetList) -> Response {
    execute_budget(
        db,
        actor,
        input.company.into_inner().as_str(),
        &BudgetCommand::List {
            year: input.year,
            account: optional_text(input.account),
            month: budget_month(input.month)?,
        },
    )
}

fn call_budget_set(db: &Db, actor: &Actor, input: inputs::BudgetSet) -> Response {
    execute_budget(
        db,
        actor,
        input.company.into_inner().as_str(),
        &BudgetCommand::Set(cli::BudgetSetArgs {
            account: input.account.to_string(),
            year: input.year,
            month: budget_month(input.month)?,
            amount: optional_text(input.amount),
            annual: optional_text(input.annual),
            currency: budget_currency(optional_text(input.currency)),
            notes: input.notes,
        }),
    )
}

fn call_budget_delete(db: &Db, actor: &Actor, input: inputs::BudgetDelete) -> Response {
    execute_budget(
        db,
        actor,
        input.company.into_inner().as_str(),
        &BudgetCommand::Delete {
            account: input.account.to_string(),
            year: input.year,
            month: budget_month(input.month)?,
            currency: budget_currency(optional_text(input.currency)),
            force: true,
        },
    )
}

fn call_txn_reconcile(db: &Db) -> Response {
    unpaged(commands::txn::execute_reconcile_json(db))
}

fn call_period_close(db: &Db, actor: &Actor, input: inputs::PeriodClose) -> Response {
    execute_period(
        db,
        actor,
        input.company.into_inner().as_str(),
        &PeriodCommand::Close {
            through: input.through.to_string(),
        },
    )
}

fn call_period_reopen(db: &Db, actor: &Actor, input: inputs::PeriodReopen) -> Response {
    execute_period(
        db,
        actor,
        input.company.into_inner().as_str(),
        &PeriodCommand::Reopen {
            reason: input.reason,
        },
    )
}

fn call_period_list(db: &Db, actor: &Actor, input: inputs::CompanyScope) -> Response {
    execute_period(
        db,
        actor,
        input.company.into_inner().as_str(),
        &PeriodCommand::List,
    )
}

fn call_verify(db: &Db) -> Response {
    unpaged(commands::verify::execute_json(db))
}

fn summary(db: &Db, input: inputs::FinancialSummary, actor: &Actor) -> Response {
    let options = commands::summary::SummaryArgs {
        from: optional_text(input.options.from),
        to: optional_text(input.options.to),
        compare_from: optional_text(input.options.compare_from),
        compare_to: optional_text(input.options.compare_to),
        currency: optional_text(input.options.currency),
        cash_account: text_list(input.options.cash_account),
        receivable_account: text_list(input.options.receivable_account),
        payable_account: text_list(input.options.payable_account),
    };
    let cli = cli::Cli::for_mcp(
        Command::Summary(options.clone()),
        input.chart,
        input.chart_all,
    );
    let mut envelope =
        commands::summary::execute_json(&cli, db, input.company.into_inner().as_str(), &options)?;
    if let Some(agent) = actor.agent_uri() {
        envelope["meta"]["agent"] = Value::String(agent.to_owned());
    }
    Ok((envelope, None))
}

fn execute_txn(db: &Db, actor: &Actor, company: &str, command: TxnCommand) -> Response {
    let cli = cli::Cli::for_mcp(
        Command::Txn(cli::TxnArgs {
            company: None,
            command: Box::new(command),
        }),
        false,
        false,
    );
    let Command::Txn(args) = &cli.command else {
        return Err(CliError::General("invalid transaction request".into()));
    };
    commands::txn::execute_json_with_pagination(&cli, db, company, &args.command, actor)
}

fn execute_security(db: &Db, actor: &Actor, company: &str, command: &SecurityCommand) -> Response {
    unpaged(commands::investment::execute_security(
        db, company, command, actor,
    ))
}

fn execute_investment(
    db: &Db,
    actor: &Actor,
    company: &str,
    command: &InvestmentCommand,
) -> Response {
    unpaged(commands::investment::execute_investment(
        db, company, command, actor,
    ))
}

fn execute_account(db: &Db, actor: &Actor, company: &str, command: &AccountCommand) -> Response {
    unpaged(commands::account::execute_json_with_actor(
        db, company, command, actor,
    ))
}

fn execute_budget(db: &Db, actor: &Actor, company: &str, command: &BudgetCommand) -> Response {
    unpaged(commands::budget::execute_json(db, company, command, actor))
}

fn execute_period(db: &Db, actor: &Actor, company: &str, command: &PeriodCommand) -> Response {
    unpaged(encode(commands::period::execute(
        db, company, command, actor,
    )?))
}

fn audit_log(db: &Db, input: inputs::AuditLog) -> Response {
    let command = AuditCommand::Log {
        agent: optional_text(input.agent),
        company: optional_text(input.company),
        entity: input.entity,
        action: input.action,
        limit: input.limit.as_ref().map_or(50, PageLimit::get),
        offset: input.offset.as_ref().map_or(0, PageOffset::get),
    };
    let data = commands::audit::execute_json(db, &command)?;
    let pagination =
        commands::audit::pagination(db, &command, data.as_array().map_or(0, Vec::len))?;
    Ok((data, Some(encode(pagination)?)))
}

fn clear(db: &Db, actor: &Actor, input: inputs::ClearInput) -> Response {
    use inputs::TxnClear::{Batch, Entry, Reference};

    let (company, args, batch) = match input.0 {
        Entry {
            company,
            transaction_id,
            entry,
            amount,
            date,
            statement,
            status,
        } => (
            company,
            cli::TxnClearArgs {
                transaction_id: Some(transaction_id.get()),
                entry: Some(entry.get()),
                reference: None,
                account: None,
                from_file: None,
                amount: optional_text(amount),
                date: optional_text(date),
                statement,
                status: status.map_or(cli::ClearanceArg::Cleared, Into::into),
            },
            None,
        ),
        Reference {
            company,
            reference,
            account,
            amount,
            date,
            statement,
            status,
        } => (
            company,
            cli::TxnClearArgs {
                transaction_id: None,
                entry: None,
                reference: Some(reference.to_string()),
                account: Some(account.to_string()),
                from_file: None,
                amount: optional_text(amount),
                date: optional_text(date),
                statement,
                status: status.map_or(cli::ClearanceArg::Cleared, Into::into),
            },
            None,
        ),
        Batch {
            company,
            account,
            batch,
            date,
            statement,
            status,
        } => (
            company,
            cli::TxnClearArgs {
                transaction_id: None,
                entry: None,
                reference: None,
                account: Some(account.to_string()),
                from_file: None,
                amount: None,
                date: optional_text(date),
                statement,
                status: status.map_or(cli::ClearanceArg::Cleared, Into::into),
            },
            Some(
                batch
                    .into_vec()
                    .into_iter()
                    .zip(1_u64..)
                    .map(|(item, line)| commands::txn_clear::BatchLine {
                        line,
                        reference: item.reference.to_string(),
                        amount: optional_text(item.amount),
                    })
                    .collect::<Vec<_>>(),
            ),
        ),
    };
    unpaged(commands::txn_clear::execute_json(
        db,
        company.into_inner().as_str(),
        &args,
        actor,
        batch.as_deref(),
    ))
}

fn posting(db: &Db, input: inputs::TxnPost, actor: &Actor, mode: PostingMode) -> Response {
    let currency = input.currency.map_or(Currency::USD, Text::into_inner);
    let lines = |lines: super::input_values::NonEmpty<inputs::Line>| {
        lines
            .into_vec()
            .into_iter()
            .map(|line| {
                commands::txn::structured_posting_line(
                    line.account.to_string(),
                    line.amount.into_inner().as_str(),
                    line.memo,
                    currency,
                )
            })
            .collect::<Result<_, CliError>>()
    };
    let request = PostingRequest {
        description: input.description,
        currency,
        date: input.date.map_or_else(
            || chrono::Local::now().date_naive(),
            |date| date.into_inner().date(),
        ),
        debit: lines(input.debits)?,
        credit: lines(input.credits)?,
        metadata: input.metadata,
        annotations: input.meta.into_iter().collect(),
        correlate: input.correlate.map(|id| id.get()),
        reference: input.reference.map(Text::into_inner),
        on_conflict: input
            .on_conflict
            .map_or(cli::OnConflictArg::Error, Into::into),
        tax: commands::txn::parse_tax_args(&input.tax)?,
    };
    unpaged(commands::txn::execute_posting_json(
        db,
        input.company.into_inner().as_str(),
        &request,
        actor,
        mode,
    ))
}

fn list_limit(limit: Option<&PageLimit>) -> Result<cli::TxnPageLimit, CliError> {
    match limit.map(PageLimit::get) {
        None => Ok(cli::TxnPageLimit::Rows(50)),
        Some(-1) => Ok(cli::TxnPageLimit::All),
        Some(limit) => u32::try_from(limit)
            .map(cli::TxnPageLimit::Rows)
            .map_err(|_| CliError::Usage("limit must be a nonnegative row count or -1".into())),
    }
}

fn budget_month(month: Option<Month>) -> Result<Option<cli::BudgetMonth>, CliError> {
    month
        .map(|month| month.get().to_string().parse())
        .transpose()
}

fn budget_currency(currency: Option<String>) -> String {
    currency
        .or_else(|| std::env::var("BEANKEEPER_CURRENCY").ok())
        .unwrap_or_else(|| Currency::USD.to_string())
}

pub(super) fn optional_text(value: Option<impl std::fmt::Display>) -> Option<String> {
    value.map(|value| value.to_string())
}

pub(super) fn text_list(values: Vec<impl std::fmt::Display>) -> Vec<String> {
    values.into_iter().map(|value| value.to_string()).collect()
}

pub(super) fn unpaged(data: Result<Value, CliError>) -> Response {
    data.map(|data| (data, None))
}

fn encode(data: impl serde::Serialize) -> Result<Value, CliError> {
    serde_json::to_value(data).map_err(|error| CliError::General(error.to_string()))
}

macro_rules! convert {
    ($input:ident => $output:path { $($variant:ident),+ $(,)? }) => {
        impl From<inputs::$input> for $output {
            fn from(value: inputs::$input) -> Self {
                match value {
                    $(inputs::$input::$variant => Self::$variant),+
                }
            }
        }
    };
}

convert!(OnConflictArg => cli::OnConflictArg { Error, Skip });
convert!(AccountTypeArg => cli::AccountTypeArg { Asset, Liability, Equity, Revenue, Expense });
convert!(DirectionArg => cli::DirectionArg { Debit, Credit });
convert!(ClearanceArg => cli::ClearanceArg { Uncleared, Cleared, Reconciled });
convert!(PeriodArg => commands::report_periods::PeriodArg { Month, Quarter, Year });
convert!(TrendMetricArg => commands::report_trend::TrendMetric { Spending, NetWorth });
convert!(AgingMethodArg => cli::AgingMethodArg { Fifo, Specific });
convert!(ReconciliationModeArg => cli::ReconciliationMode { Bank, Settlement });
