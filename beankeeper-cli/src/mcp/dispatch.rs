//! Translate decoded MCP inputs to the shared command entry points.

use beankeeper::core::LotMethod;
use beankeeper::types::Currency;
use serde_json::Value;

use super::input_values::{DecimalAmount, Month, PageLimit, PageOffset, Text};
use super::inputs;
use crate::capability::Capability;
use crate::cli::{
    self, AccountCommand, ActorFilterArg, BudgetCommand, Command, CompanyCommand,
    InvestmentCommand, PeriodCommand, SecurityCommand, TradeArgs, TxnCommand, TxnProposalCommand,
};
use crate::commands;
use crate::commands::txn::{PostingMode, PostingRequest};
use crate::confirmation::Confirmed;
use crate::db::audit::setup::{
    books::{self, BookChange},
    openings::{self, OpeningLine, OpeningRequest},
    statement_batches,
};
use crate::db::{Actor, Db, audit::MetadataTarget, metadata};
use crate::error::CliError;
use crate::operation::{Operation, Permission};
use crate::output::json::deleted_data;

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
    LedgerSetupStatus(Empty),
    LedgerSetupBookShow(CompanyScope),
    LedgerSetupStatementPreview(LedgerSetupStatementPreview),
    LedgerSetupStatementCommit(LedgerSetupStatementCommit),
    LedgerSetupStatementReceipt(LedgerSetupStatementReceipt),
    LedgerSetupBegin(LedgerSetupBegin),
    LedgerSetupFinish(LedgerSetupEnd),
    LedgerSetupRevoke(LedgerSetupEnd),
    LedgerSetupBookCreate(LedgerSetupBookCreate),
    LedgerSetupBookInitialize(LedgerSetupBookInitialize),
    LedgerSetupBookUpdate(LedgerSetupBookUpdate),
    LedgerSetupChartApply(LedgerSetupChartApply),
    LedgerSetupAccountAdd(LedgerSetupAccountAdd),
    LedgerSetupOpeningPreview(LedgerSetupOpeningPreview),
    LedgerSetupOpeningReserve(LedgerSetupOpeningReserve),
    SecurityCreate(SecurityCreate),
    SecurityList(CompanyScope),
    SecurityPrice(SecurityPrice),
    InvestmentBuy(InvestmentBuy),
    InvestmentSell(InvestmentSell),
    InvestmentLots(InvestmentLots),
    InvestmentHoldings(InvestmentHoldings),
    CompanyList(Empty),
    CompanyShow(CompanyShow),
    CompanyCreate(CompanyCreate),
    CompanyDelete(CompanyShow),
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
    pub(super) fn writes_ledger(&self) -> bool {
        let operation = self.operation();
        !matches!(
            operation.permission(),
            Permission::Capability(Capability::Read)
                | Permission::ProposalList
                | Permission::SetupStatus
        ) && !matches!(
            operation,
            Operation::LedgerSetupOpeningPreview | Operation::TxnReconcile
        )
    }

    pub(super) fn execute(self, db: &Db, actor: &Actor) -> Response {
        match self {
            Self::Summary(input) => summary(db, input, actor),
            Self::LedgerSetupStatus(_) => setup_status(db),
            Self::LedgerSetupBookShow(input) => setup_book_show(db, input),
            Self::LedgerSetupStatementPreview(input) => setup_statement_preview(db, actor, input),
            Self::LedgerSetupStatementCommit(input) => setup_statement_commit(db, actor, input),
            Self::LedgerSetupStatementReceipt(input) => setup_statement_receipt(db, input),
            Self::LedgerSetupBegin(input) => setup_begin(db, actor, &input),
            Self::LedgerSetupFinish(input) => unpaged(commands::setup::end_json(
                db,
                actor,
                input.generation,
                crate::db::audit::setup::End::Finish,
            )),
            Self::LedgerSetupRevoke(input) => unpaged(commands::setup::end_json(
                db,
                actor,
                input.generation,
                crate::db::audit::setup::End::Revoke,
            )),
            Self::LedgerSetupBookCreate(input) => setup_book_create(db, actor, input),
            Self::LedgerSetupBookInitialize(input) => setup_book_change(
                db,
                actor,
                input.target,
                BookChange::Initialize(input.settings.into()),
            ),
            Self::LedgerSetupBookUpdate(input) => setup_book_change(
                db,
                actor,
                input.target,
                BookChange::Supersede {
                    revision: input.supersedes,
                    settings: input.settings.into(),
                },
            ),
            Self::LedgerSetupChartApply(input) => setup_book_change(
                db,
                actor,
                input.target,
                BookChange::ApplyChart(input.template),
            ),
            Self::LedgerSetupAccountAdd(input) => setup_book_change(
                db,
                actor,
                input.target,
                BookChange::AddAccount {
                    code: &input.code.into_inner(),
                    name: &input.name,
                    account_type: input.account_type.into_inner(),
                },
            ),
            Self::LedgerSetupOpeningPreview(input) => setup_opening_preview(db, actor, input),
            Self::LedgerSetupOpeningReserve(input) => setup_opening_reserve(db, actor, input),
            Self::Report(input) => super::report_input::execute(db, input),
            Self::SecurityCreate(input) => call_security_create(db, actor, input),
            Self::SecurityList(input) => call_security_list(db, actor, input),
            Self::SecurityPrice(input) => call_security_price(db, actor, input),
            Self::InvestmentBuy(input) => call_investment_buy(db, actor, input),
            Self::InvestmentSell(input) => call_investment_sell(db, actor, input),
            Self::InvestmentLots(input) => call_investment_lots(db, actor, input),
            Self::InvestmentHoldings(input) => call_investment_holdings(db, actor, input),
            Self::CompanyList(_) => call_company_list(db),
            Self::CompanyShow(input) => call_company_show(db, input),
            Self::CompanyCreate(input) => call_company_create(db, actor, input),
            Self::CompanyDelete(input) => call_company_delete(db, actor, input),
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

fn setup_book_create(db: &Db, actor: &Actor, input: inputs::LedgerSetupBookCreate) -> Response {
    setup_book_change(
        db,
        actor,
        input.target,
        BookChange::Create {
            name: &input.name,
            settings: input.settings.into(),
        },
    )
}

fn setup_status(db: &Db) -> Response {
    let state = commands::setup::status(db)?;
    unpaged(serde_json::to_value(state).map_err(|error| CliError::General(error.to_string())))
}

fn setup_book_show(db: &Db, input: inputs::CompanyScope) -> Response {
    let company = input.company.into_inner();
    let facts = commands::setup::book_facts(db, &company)?;
    unpaged(serde_json::to_value(facts).map_err(|error| CliError::General(error.to_string())))
}

fn setup_begin(db: &Db, actor: &Actor, input: &inputs::LedgerSetupBegin) -> Response {
    unpaged(commands::setup::begin_json(
        db,
        actor,
        &input.name,
        &input.public_key,
        input.ttl.as_deref().unwrap_or(commands::setup::DEFAULT_TTL),
    ))
}

fn setup_statement_preview(
    db: &Db,
    actor: &Actor,
    input: inputs::LedgerSetupStatementPreview,
) -> Response {
    let (reference, request) = input.into_request()?;
    let preview = statement_batches::preview(db, actor, &reference, &request)?;
    unpaged(serde_json::to_value(preview).map_err(|error| CliError::General(error.to_string())))
}

fn setup_statement_commit(
    db: &Db,
    actor: &Actor,
    input: inputs::LedgerSetupStatementCommit,
) -> Response {
    let receipt = statement_batches::commit(
        db,
        actor,
        &input.company.into_inner(),
        input.batch_id,
        &input.preview_sha256.into_inner(),
    )?;
    unpaged(serde_json::to_value(receipt).map_err(|error| CliError::General(error.to_string())))
}

fn setup_statement_receipt(db: &Db, input: inputs::LedgerSetupStatementReceipt) -> Response {
    let receipt =
        statement_batches::get_receipt(db.read(), &input.company.into_inner(), input.batch_id)?;
    unpaged(serde_json::to_value(receipt).map_err(|error| CliError::General(error.to_string())))
}

fn setup_book_change(
    db: &Db,
    actor: &Actor,
    target: inputs::SetupTarget,
    change: BookChange<'_>,
) -> Response {
    unpaged(books::apply(
        db,
        actor,
        target.generation,
        &target.reference.into_inner(),
        &target.company.into_inner(),
        change,
    ))
}

fn setup_opening_preview(
    db: &Db,
    actor: &Actor,
    input: inputs::LedgerSetupOpeningPreview,
) -> Response {
    let request = opening_request(input)?;
    let preview = openings::preview(db, actor, &request)?;
    unpaged(serde_json::to_value(preview).map_err(|error| CliError::General(error.to_string())))
}

fn setup_opening_reserve(
    db: &Db,
    actor: &Actor,
    input: inputs::LedgerSetupOpeningReserve,
) -> Response {
    let request = opening_request(input.opening)?;
    unpaged(openings::reserve(
        db,
        actor,
        &request,
        &input.reference.into_inner(),
        &input.fingerprint.into_inner(),
    ))
}

fn opening_request(input: inputs::LedgerSetupOpeningPreview) -> Result<OpeningRequest, CliError> {
    let currency = input.currency.into_inner();
    let lines = |side: super::input_values::NonEmpty<inputs::Line>| {
        side.into_vec()
            .into_iter()
            .map(|line| {
                let amount = line.amount.into_inner();
                let minor = commands::txn::parse_amount_to_minor(amount.as_str(), currency)?;
                OpeningLine::new(line.account.into_inner(), minor, currency, line.memo)
            })
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(OpeningRequest {
        company: input.company.into_inner(),
        generation: input.generation,
        date: input.date.into_inner().date(),
        currency,
        description: input
            .description
            .unwrap_or_else(|| openings::DEFAULT_DESCRIPTION.to_string()),
        debits: lines(input.debits)?,
        credits: lines(input.credits)?,
    })
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

fn call_company_show(db: &Db, input: inputs::CompanyShow) -> Response {
    unpaged(commands::company::execute_json(
        db,
        &CompanyCommand::Show {
            slug: input.slug.into_inner(),
        },
    ))
}

fn call_company_create(db: &Db, actor: &Actor, input: inputs::CompanyCreate) -> Response {
    let row = commands::company::create(
        db,
        actor,
        &input.slug.into_inner(),
        &input.name,
        input.description.as_deref(),
    )?;
    unpaged(encode([row]))
}

fn call_company_delete(db: &Db, actor: &Actor, input: inputs::CompanyShow) -> Response {
    let slug = input.slug.into_inner();
    commands::company::delete(db, actor, &slug, Confirmed::explicit())?;
    unpaged(encode(deleted_data(slug.as_str())))
}

fn call_account_list(db: &Db, actor: &Actor, input: inputs::AccountList) -> Response {
    execute_account(
        db,
        actor,
        &input.company.into_inner(),
        &AccountCommand::List {
            account_type: input.account_type,
            name: input.name,
            with_balances: input.with_balances,
            from: input.from.map(|date| date.into_inner().date()),
            to: input.to.map(|date| date.into_inner().date()),
            currency: input.currency.map(|value| value.into_inner().into()),
        },
    )
}

fn call_account_show(db: &Db, actor: &Actor, input: inputs::AccountShow) -> Response {
    execute_account(
        db,
        actor,
        &input.company.into_inner(),
        &AccountCommand::Show {
            code: input.code.to_string(),
        },
    )
}

fn call_account_create(db: &Db, actor: &Actor, input: inputs::AccountCreate) -> Response {
    execute_account(
        db,
        actor,
        &input.company.into_inner(),
        &AccountCommand::Create {
            code: input.code.to_string(),
            name: input.name,
            account_type: input.account_type,
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
            id: input.id.into_inner(),
        }),
    )
}

fn call_txn_reject(db: &Db, actor: &Actor, input: inputs::TxnReject) -> Response {
    execute_txn(
        db,
        actor,
        input.company.into_inner().as_str(),
        TxnCommand::Proposal(TxnProposalCommand::Reject {
            id: input.id.into_inner(),
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
            id: input.id,
            date: calendar_arg(input.date),
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
            MetadataTarget::Id(id),
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
            MetadataTarget::Id(id),
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
            from: input.from.map(|date| date.into_inner().date()),
            to: input.to.map(|date| date.into_inner().date()),
            limit: list_limit(input.limit.as_ref())?,
            offset: u32::try_from(input.offset.as_ref().map_or(0, PageOffset::get))
                .map_err(|_| CliError::Usage("offset must be a nonnegative row count".into()))?,
            description: input.description,
            amount_gt: optional_text(input.amount_gt),
            amount_lt: optional_text(input.amount_lt),
            amount_eq: optional_text(input.amount_eq),
            currency: input.currency.map(|currency| currency.into_inner().into()),
            reference: optional_text(input.reference),
            metadata: input.metadata,
            meta: input.meta,
            tax_category: input.tax_category,
            direction: input.direction,
            count: input.count,
        }),
    )
}

fn call_txn_show(db: &Db, actor: &Actor, input: inputs::TxnShow) -> Response {
    execute_txn(
        db,
        actor,
        input.company.into_inner().as_str(),
        TxnCommand::Show { id: input.id },
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
            month: budget_month(input.month),
        },
    )
}

fn call_budget_set(db: &Db, actor: &Actor, input: inputs::BudgetSet) -> Response {
    use commands::budget::{BudgetScope, BudgetSetRequest};

    let currency = budget_currency(input.currency.map(Text::into_inner))?;
    let amount = input.amount.map(Text::into_inner);
    let annual = input.annual.map(Text::into_inner);
    let scope = BudgetScope::from_flags(
        budget_month(input.month),
        amount.as_ref().map(DecimalAmount::as_str),
        annual.as_ref().map(DecimalAmount::as_str),
    )?;
    unpaged(commands::budget::execute_set_json(
        db,
        input.company.into_inner().as_str(),
        &BudgetSetRequest {
            account: input.account.into_inner().as_str(),
            year: input.year,
            scope,
            currency,
            notes: input.notes.as_deref(),
        },
        actor,
    ))
}

fn call_budget_delete(db: &Db, actor: &Actor, input: inputs::BudgetDelete) -> Response {
    execute_budget(
        db,
        actor,
        input.company.into_inner().as_str(),
        &BudgetCommand::Delete {
            account: input.account.to_string(),
            year: input.year,
            month: budget_month(input.month),
            currency: budget_currency(input.currency.map(Text::into_inner))?.into(),
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
        from: calendar_arg(input.options.from),
        to: calendar_arg(input.options.to),
        compare_from: calendar_arg(input.options.compare_from),
        compare_to: calendar_arg(input.options.compare_to),
        currency: input
            .options
            .currency
            .map(|currency| currency.into_inner().into()),
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

fn execute_account(
    db: &Db,
    actor: &Actor,
    company: &crate::db::companies::CompanySlug,
    command: &AccountCommand,
) -> Response {
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
    let agent = input.agent.map(Text::into_inner);
    let company = input.company.map(Text::into_inner);
    let params = crate::db::ListAuditParams {
        actor: ActorFilterArg::resolve_optional(input.actor.as_ref())?,
        agent_uri: agent.as_ref().map(agent_uri::AgentUri::as_str),
        company_slug: company.as_ref(),
        entity: input.entity,
        action: input.action,
        limit: input.limit.as_ref().map_or(50, PageLimit::get),
        offset: input.offset.as_ref().map_or(0, PageOffset::get),
    };
    let data = commands::audit::execute_json(db, &params)?;
    let pagination = commands::audit::pagination(db, &params, data.as_array().map_or(0, Vec::len))?;
    Ok((data, Some(encode(pagination)?)))
}

fn clear(db: &Db, actor: &Actor, input: inputs::ClearInput) -> Response {
    use commands::txn_clear::{BatchLine, ClearRequestTarget, EntrySelector, execute_json};
    use inputs::TxnClear::{Batch, Entry, Reference};

    unpaged(match input.0 {
        Entry {
            company,
            transaction_id,
            entry,
            amount,
            date,
            statement,
            status,
        } => execute_json(
            db,
            &company.into_inner(),
            actor,
            ClearRequestTarget::Entry {
                selector: EntrySelector::Id {
                    transaction_id,
                    entry_id: entry,
                },
                amount: amount.map(Text::into_inner),
            },
            clear_options(date, statement.as_deref(), status),
        ),
        Reference {
            company,
            reference,
            account,
            amount,
            date,
            statement,
            status,
        } => execute_json(
            db,
            &company.into_inner(),
            actor,
            ClearRequestTarget::Entry {
                selector: EntrySelector::Reference {
                    reference: reference.into_inner().as_str(),
                    account: account.into_inner().as_str(),
                },
                amount: amount.map(Text::into_inner),
            },
            clear_options(date, statement.as_deref(), status),
        ),
        Batch {
            company,
            account,
            batch,
            date,
            statement,
            status,
        } => execute_json(
            db,
            &company.into_inner(),
            actor,
            ClearRequestTarget::Batch {
                account: account.into_inner().as_str(),
                lines: batch
                    .into_vec()
                    .into_iter()
                    .zip(1_u64..)
                    .map(|(item, line)| BatchLine {
                        line,
                        reference: item.reference.into_inner(),
                        amount: item.amount.map(Text::into_inner),
                    })
                    .collect(),
            },
            clear_options(date, statement.as_deref(), status),
        ),
    })
}

pub(super) fn calendar_arg(
    value: Option<Text<super::input_values::CalendarDate>>,
) -> Option<cli::CalendarDateArg> {
    value.map(|date| date.into_inner().date().into())
}

fn clear_options(
    date: Option<Text<super::input_values::CalendarDate>>,
    statement: Option<&str>,
    status: Option<inputs::ClearanceArg>,
) -> commands::txn_clear::ClearOptions<'_> {
    commands::txn_clear::ClearOptions {
        date: date.map(|date| date.into_inner().date()),
        statement,
        status: status.map_or(cli::ClearanceArg::Cleared, Into::into),
    }
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
        correlate: input.correlate,
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

fn budget_month(month: Option<Month>) -> Option<cli::BudgetMonth> {
    month.map(|month| month.into_inner().into())
}

fn budget_currency(currency: Option<Currency>) -> Result<Currency, CliError> {
    if let Some(currency) = currency {
        return Ok(currency);
    }
    match std::env::var("BEANKEEPER_CURRENCY") {
        Ok(default) => cli::parse_currency_arg(&default)
            .map_err(|error| CliError::Validation(error.to_string())),
        Err(_) => Ok(Currency::USD),
    }
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
convert!(ClearanceArg => cli::ClearanceArg { Uncleared, Cleared, Reconciled });
convert!(PeriodArg => commands::report_periods::PeriodArg { Month, Quarter, Year });
convert!(TrendMetricArg => commands::report_trend::TrendMetric { Spending, NetWorth });
convert!(AgingMethodArg => cli::AgingMethodArg { Fifo, Specific });
convert!(ReconciliationModeArg => cli::ReconciliationMode { Bank, Settlement });
