//! Translate decoded MCP parameters to validated ledger requests and serialize
//! typed responses at the wire boundary.

use serde::Serialize;
use serde_json::Value;

use super::{input_values::Text, inputs};
use crate::{
    commands::{
        account::AccountRequest,
        audit::ReceiptRequest,
        budget::{BudgetRequest, BudgetSetInput},
        company::CompanyRequest,
        investment::InvestmentRequest,
        period::PeriodRequest,
        rule::RuleRequest,
        setup::SetupRequest,
        standing::StandingRequest,
        txn::TxnRequest,
    },
    confirmation::ExplicitDeleteCall,
    db::Actor,
    error::CliError,
    output::json::Pagination,
    request::{LedgerRequest, LedgerResponse},
};

macro_rules! convert {
    ($($function:ident($input:ident) => $family:ident($value:ident => $convert:expr)),+ $(,)?) => {
        $(pub(super) fn $function($value: inputs::$input) -> LedgerRequest {
            LedgerRequest::$family($convert)
        })+
    };
}

macro_rules! try_convert {
    ($($function:ident($input:ident) => $family:ident($value:ident => $convert:expr)),+ $(,)?) => {
        $(pub(super) fn $function($value: inputs::$input) -> Result<LedgerRequest, CliError> {
            Ok(LedgerRequest::$family($convert))
        })+
    };
}

convert! {
    ledger_setup_status(Empty) => Setup(_input => SetupRequest::Status),
    ledger_setup_book_show(CompanyScope) => Setup(input => SetupRequest::BookShow(input.company.into_inner())),
    ledger_setup_statement_commit(LedgerSetupStatementCommit) => Setup(input => input.into()),
    ledger_setup_statement_receipt(LedgerSetupStatementReceipt) => Setup(input => input.into()),
    ledger_setup_finish(LedgerSetupEnd) => Setup(input => SetupRequest::Finish(input.generation)),
    ledger_setup_revoke(LedgerSetupEnd) => Setup(input => SetupRequest::Revoke(input.generation)),
    ledger_setup_book_create(LedgerSetupBookCreate) => Setup(input => input.into()),
    ledger_setup_book_initialize(LedgerSetupBookInitialize) => Setup(input => input.into()),
    ledger_setup_book_update(LedgerSetupBookUpdate) => Setup(input => input.into()),
    ledger_setup_chart_apply(LedgerSetupChartApply) => Setup(input => input.into()),
    security_list(CompanyScope) => Investment(input => InvestmentRequest::SecurityList(input.company.into_inner())),
    investment_lots(InvestmentLots) => Investment(input => input.into()),
    investment_holdings(InvestmentHoldings) => Investment(input => input.into()),
    company_create(CompanyCreate) => Company(input => CompanyRequest::Create {
        slug: input.slug.into_inner(), name: input.name, description: input.description,
    }),
    company_list(Empty) => Company(_input => CompanyRequest::List),
    company_show(CompanyShow) => Company(input => CompanyRequest::Show(input.slug.into_inner())),
    company_delete(CompanyShow) => Company(input => CompanyRequest::Delete(input.slug.into_inner(), ExplicitDeleteCall.into())),
    account_show(AccountShow) => Account(input => AccountRequest::Show(input.company.into_inner(), input.code.into_inner())),
    account_create(AccountCreate) => Account(input => AccountRequest::Create {
        company: input.company.into_inner(), code: input.code.into_inner(), name: input.name.into_inner(),
        account_type: input.account_type, default_tax_category: input.default_tax_category,
    }),
    account_rename(AccountRename) => Account(input => AccountRequest::Rename(
        input.company.into_inner(), input.code.into_inner(), input.name.into_inner(),
    )),
    txn_reverse(TxnReverse) => Transaction(input => input.into()),
    txn_settle_hold(TxnSettleHold) => Transaction(input => input.into()),
    txn_proposals(TxnProposals) => Transaction(input => input.into()),
    txn_approve(TxnApprove) => Transaction(input => input.into()),
    txn_reject(TxnReject) => Transaction(input => input.into()),
    txn_show(TxnShow) => Transaction(input => input.into()),
    txn_reconcile(Empty) => Transaction(_input => TxnRequest::Reconcile),
    budget_list(BudgetList) => Budget(input => BudgetRequest::List {
        company: input.company.into_inner(), year: input.year,
        account: input.account.map(Text::into_inner), month: input.month.map(super::input_values::Month::into_inner),
    }),
    rule_list(CompanyScope) => Rule(input => RuleRequest::List(input.company.into_inner())),
    rule_create(RuleCreate) => Rule(input => RuleRequest::Create(input.company.into_inner(),
        (input.contains.into_inner(), input.account.into_inner(), input.priority).into())),
    rule_amend(RuleAmend) => Rule(input => RuleRequest::Amend(input.company.into_inner(), input.id.into_inner(),
        (input.contains.into_inner(), input.account.into_inner(), input.priority).into())),
    rule_match(RuleMatch) => Rule(input => RuleRequest::Match(input.company.into_inner(), input.description)),
    rule_undo(RuleIdInput) => Rule(input => RuleRequest::Undo(input.company.into_inner(), input.id.into_inner())),
    rule_delete(RuleIdInput) => Rule(input => RuleRequest::Delete(input.company.into_inner(),
        input.id.into_inner(), ExplicitDeleteCall.into())),
    bookkeeping_policy_show(CompanyScope) => Standing(input => StandingRequest::ShowBookkeeping(input.company.into_inner())),
    bookkeeping_policy_set(BookkeepingPolicySet) => Standing(input => StandingRequest::SetBookkeeping(input.company.into_inner(), input.policy)),
    inform_preference_show(CompanyScope) => Standing(input => StandingRequest::ShowInform(input.company.into_inner())),
    inform_preference_set(InformPreferenceSet) => Standing(input => StandingRequest::SetInform(input.company.into_inner(), input.preference)),
    period_close(PeriodClose) => Period(input => input.into()),
    period_list(CompanyScope) => Period(input => PeriodRequest::List(input.company.into_inner())),
    audit_log(AuditLog) => AuditLog(input => input.into()),
    audit_undo(AuditUndo) => AuditReceipts(input => ReceiptRequest::Undo(input.company.into_inner(), input.receipt)),
}

try_convert! {
    txn_link(TxnLink) => Transaction(input => input.try_into()?),
    txn_recategorize(TxnRecategorize) => Transaction(input => input.try_into()?),
    period_reopen(PeriodReopen) => Period(input => input.try_into()?),
    financial_summary(FinancialSummary) => Summary(input => input.try_into()?),
    ledger_setup_statement_preview(LedgerSetupStatementPreview) => Setup(input => input.try_into()?),
    ledger_setup_begin(LedgerSetupBegin) => Setup(input => input.try_into()?),
    ledger_setup_account_add(LedgerSetupAccountAdd) => Setup(input => input.into()),
    ledger_setup_opening_post(LedgerSetupOpeningPost) => Setup(input => input.try_into()?),
    ledger_setup_statement_import(LedgerSetupStatementImport) => Setup(input => input.try_into()?),
    ledger_setup_opening_preview(LedgerSetupOpeningPreview) => Setup(input => input.try_into()?),
    ledger_setup_opening_reserve(LedgerSetupOpeningReserve) => Setup(input => input.try_into()?),
    security_create(SecurityCreate) => Investment(input => input.try_into()?),
    security_price(SecurityPrice) => Investment(input => input.try_into()?),
    investment_buy(InvestmentBuy) => Investment(input => input.try_into()?),
    investment_sell(InvestmentSell) => Investment(input => input.try_into()?),
    account_list(AccountList) => Account(input => AccountRequest::List {
        company: input.company.into_inner(), account_type: input.account_type, name: input.name,
        totals: (
            input.with_balances,
            input.from.map(|date| date.into_inner().date()),
            input.to.map(|date| date.into_inner().date()),
            input.currency,
        ).try_into()?,
    }),
    txn_post(TxnPost) => Transaction(input => input.try_into()?),
    txn_propose(TxnPost) => Transaction(input => TxnRequest::propose_mcp(input)?),
    txn_meta_set(TxnMetaSet) => Transaction(input => input.try_into()?),
    txn_meta_unset(TxnMetaUnset) => Transaction(input => input.try_into()?),
    txn_list(TxnList) => Transaction(input => input.try_into()?),
    txn_clear(ClearInput) => Transaction(input => input.try_into()?),
    report(Report) => Report(input => input.try_into()?),
    budget_set(BudgetSet) => Budget(input => BudgetRequest::Set(input.company.into_inner(), BudgetSetInput {
        account: input.account.into_inner(),
        year: input.year,
        month: input.month.map(super::input_values::Month::into_inner),
        amount: input.amount.map(Text::into_inner),
        annual: input.annual.map(Text::into_inner),
        currency: input.currency,
        notes: input.notes,
    }.try_into()?)),
    budget_delete(BudgetDelete) => Budget(input => BudgetRequest::Delete(
        input.company.into_inner(),
        (
            input.account.into_inner(), input.year,
            input.month.map(super::input_values::Month::into_inner),
            input.currency,
        ).try_into()?, ExplicitDeleteCall.into(),
    )),
    audit_receipts(AuditReceipts) => AuditReceipts(input => ReceiptRequest::List(
        (input.company.into_inner(), input.cursor, input.continuation, input.limit).try_into()?,
    )),
}

pub(super) const fn verify(_: inputs::Empty) -> LedgerRequest {
    LedgerRequest::Verify
}

/// Accept both infallible and fallible conversions in the generated routes.
pub(super) trait IntoRequest {
    fn into_request(self) -> Result<LedgerRequest, CliError>;
}

impl IntoRequest for LedgerRequest {
    fn into_request(self) -> Result<LedgerRequest, CliError> {
        Ok(self)
    }
}

impl IntoRequest for Result<LedgerRequest, CliError> {
    fn into_request(self) -> Result<LedgerRequest, CliError> {
        self
    }
}

pub(super) fn response(
    response: &LedgerResponse,
    actor: &Actor,
) -> Result<(Value, Option<Value>), CliError> {
    fn encode(data: impl Serialize) -> Result<Value, CliError> {
        serde_json::to_value(data)
            .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
    }

    let (data, pagination): (Value, Option<&Pagination>) = match response {
        LedgerResponse::Account(response) => (encode(response)?, None),
        LedgerResponse::AuditLog(response) => {
            (encode(response.data())?, Some(response.pagination()))
        }
        LedgerResponse::AuditReceipts(response) => (encode(response)?, None),
        LedgerResponse::Budget(response) => (encode(response)?, None),
        LedgerResponse::Company(response) => (encode(response)?, None),
        LedgerResponse::Investment(_, _, response) => (encode(response)?, None),
        LedgerResponse::Period(_, _, response) => (encode(response)?, None),
        LedgerResponse::Report(response) => (response.data()?, None),
        LedgerResponse::Rule(_, _, response) => (encode(response)?, None),
        LedgerResponse::Standing(_, _, response) => (encode(response)?, None),
        LedgerResponse::Setup(_, _, response) => (encode(response)?, None),
        LedgerResponse::Summary(response) => (encode(response.envelope(actor))?, None),
        LedgerResponse::Transaction(response) => {
            response.check_reconciliation()?;
            (encode(response)?, response.pagination())
        }
        LedgerResponse::Verify(response) => (encode(response)?, None),
    };
    Ok((data, pagination.map(encode).transpose()?))
}
