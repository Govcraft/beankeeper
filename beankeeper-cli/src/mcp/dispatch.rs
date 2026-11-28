//! Translate decoded MCP parameters to validated ledger requests and serialize
//! typed responses at the wire boundary.

use serde::Serialize;
use serde_json::Value;

use super::{input_values::Text, inputs};
use crate::{
    commands::{
        account::AccountRequest,
        audit::ReceiptRequest,
        bank_feed::BankFeedRequest,
        budget::{BudgetRequest, BudgetSetInput},
        company::CompanyRequest,
        contractor_tax::{ContractorTaxAction as Contractor, ContractorTaxRequest},
        counterparty::{CounterpartyAction as Party, CounterpartyRequest},
        disbursement::{Action as Pay, DisbursementRequest},
        document::{DocumentAction as Doc, DocumentRequest},
        group_report::GroupReportRequest,
        investment::InvestmentRequest,
        period::PeriodRequest,
        project::{ProjectCommand, ProjectRequest, Splits},
        rule::RuleRequest,
        setup::SetupRequest,
        standing::StandingRequest,
        tax::TaxRequest,
        txn::TxnRequest,
        txn_import::JournalImportRequest,
    },
    confirmation::ExplicitDeleteCall,
    db::{
        Actor,
        audit::documents::{ApprovalAction, DocumentMutation as DocChange},
        counterparties::{ContactDetails, PartyPageRequest},
    },
    error::CliError,
    output::json::Pagination,
    request::{CurrencySetting, LedgerRequest, LedgerResponse},
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
    bank_feed_connect(BankFeedConnect) => BankFeed(input => (input.company.into_inner(), BankFeedRequest::Connect(crate::db::bank_feed::ConnectArgs {
        account: input.account.into_inner(), provider_account: input.provider_account, currency: input.currency.into_inner(), environment: input.environment,
    }))),
    bank_feed_sync(BankFeedShow) => BankFeed(input => (input.company.into_inner(), BankFeedRequest::Sync(input.feed))),
    bank_feed_disconnect(BankFeedShow) => BankFeed(input => (input.company.into_inner(), BankFeedRequest::Disconnect(input.feed))),
    collection_create_link(CollectionCheckout) => PaymentCollection(input => input.into()),
    collection_apply(CollectionDelivery) => PaymentCollection(input => input.into()),
    collection_resolve_receipt(CollectionReceiptResolution) => PaymentCollection(input => input.into()),
    collection_reconcile_payout(CollectionPayout) => PaymentCollection(input => input.into()),
    collection_payouts(CompanyScope) => PaymentCollection(input => crate::commands::collection::PaymentCollectionRequest::payouts(input.company.into_inner())),
    collection_reconcile_refund(CollectionRefund) => PaymentCollection(input => input.into()),
    collection_disputes(CompanyScope) => PaymentCollection(input => crate::commands::collection::PaymentCollectionRequest::disputes(input.company.into_inner())),
    collection_refunds(CompanyScope) => PaymentCollection(input => crate::commands::collection::PaymentCollectionRequest::refunds(input.company.into_inner())),
    collection_show(CollectionShow) => PaymentCollection(input => input.into()),
    collection_exceptions(CompanyScope) => PaymentCollection(input => crate::commands::collection::PaymentCollectionRequest::exceptions(input.company.into_inner())),
    bank_feed_link(BankFeedLink) => BankFeed(input => (input.company.into_inner(), BankFeedRequest::Link(input.args))),
    bank_feed_post(BankFeedPost) => BankFeed(input => (input.company.into_inner(), BankFeedRequest::Post(input.args))),
    bank_feed_review(BankFeedReview) => BankFeed(input => (input.company.into_inner(), BankFeedRequest::Review(input.feed, input.item))),
    bank_feed_list(BankFeedList) => BankFeed(input => (input.company.into_inner(), BankFeedRequest::List(input.feed, input.page))),
    bank_feed_show(BankFeedShow) => BankFeed(input => (input.company.into_inner(), BankFeedRequest::Show(input.feed))),
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
        native_currency: input.native_currency,
    }),
    fixed_asset_show(FixedAssetAt) => FixedAsset(input => input.show()),
    fixed_asset_list(FixedAssetList) => FixedAsset(input => input.into()),
    fixed_asset_preview(FixedAssetAt) => FixedAsset(input => input.preview()),
    fixed_asset_depreciate(FixedAssetAt) => FixedAsset(input => input.depreciate()),
    fixed_asset_dispose(FixedAssetDispose) => FixedAsset(input => input.into()),
    expense_claim_receipt_show(ExpenseReceiptInput) => Expense(input => input.into()),
    expense_claim_create(ExpenseCreateInput) => Expense(input => input.into()),
    expense_claim_amend(ExpenseAmendInput) => Expense(input => input.into()),
    expense_claim_submit(ExpenseRevisionInput) => Expense(input => input.submit()),
    expense_claim_approve(ExpenseRevisionInput) => Expense(input => input.approve()),
    expense_claim_reject(ExpenseRejectInput) => Expense(input => input.into()),
    expense_claim_record_payment(ExpensePaymentInput) => Expense(input => input.into()),
    expense_claim_void(ExpenseVoidInput) => Expense(input => input.into()),
    expense_claim_show(ExpenseShowInput) => Expense(input => input.into()),
    expense_claim_list(CompanyScope) => Expense(input => crate::commands::expense_claim::ExpenseRequest {company:input.company.into_inner(),work:crate::commands::expense_claim::Work::List}),
    inventory_register(StockRegisterInput) => Inventory(input => input.into()),
    inventory_list(CompanyScope) => Inventory(input => crate::commands::inventory::InventoryRequest {company:input.company.into_inner(),work:crate::commands::inventory::Work::List}),
    inventory_show(StockItemSelector) => Inventory(input => input.into()),
    inventory_move(StockMoveInput) => Inventory(input => input.into()),
    inventory_movement(StockMovementSelector) => Inventory(input => input.into()),
    inventory_movements(StockHistoryInput) => Inventory(input => input.into()),
    inventory_match_document(StockLinkInput) => Inventory(input => input.into()),
    inventory_valuation(StockValuationInput) => Inventory(input => input.into()),
    fx_post(FxPostInput) => Fx(input => (input.company.into_inner(), crate::db::fx::Movement {
        account: input.account.into_inner(), offset: input.offset_account.into_inner(),
        direction: input.direction.into_inner(), native_minor: input.native_minor, rate: input.rate,
        date: input.date.into(), description: input.description, reference: input.reference.into_inner(),
    }).into()),
    fx_revalue(FxRevalueInput) => Fx(input => (input.company.into_inner(), crate::db::fx::Revalue {
        account: input.account.map(Text::into_inner), offset: input.fx_gain_loss_account.into_inner(),
        rate: input.rate, date: input.as_of.into(),
    }).into()),
    account_classify(AccountClassify) => Account(input => AccountRequest::Classify(
        input.company.into_inner(), input.code.into_inner(), input.cash_class.into_inner(),
    )),
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
    tax_configure(TaxConfigureInput) => Tax(input => TaxRequest::Configure(input.company.into_inner(), input.code, input.terms)),
    tax_list(CompanyScope) => Tax(input => TaxRequest::List(input.company.into_inner())),
    tax_report(TaxReportInput) => Tax(input => TaxRequest::Report(input.company.into_inner(), input.from.into_inner().date(), input.to.into_inner().date(), input.currency.unwrap_or(beankeeper::types::Currency::GBP))),
    contractor_tax_w9(ContractorTaxW9Input) => ContractorTax(input => ContractorTaxRequest::new(input.company.into_inner(), Contractor::W9Artifact { party: input.party, classification: input.classification, input: input.input })),
    contractor_tax_export(ContractorTaxExportInput) => ContractorTax(input => ContractorTaxRequest::new(input.company.into_inner(), Contractor::ExportArtifact { year: input.year, output: input.output })),
    contractor_tax_review(ContractorTaxReviewInput) => ContractorTax(input => ContractorTaxRequest::new(input.company.into_inner(), Contractor::Review(input.payment, input.expected_receipt))),
    contractor_tax_worksheet(ContractorTaxYearInput) => ContractorTax(input => ContractorTaxRequest::new(input.company.into_inner(), Contractor::Worksheet(input.year))),
    contractor_tax_preview(ContractorTaxYearInput) => ContractorTax(input => ContractorTaxRequest::new(input.company.into_inner(), Contractor::Preview(input.year))),
    disbursement_draft(DisbursementDraftInput) => Disbursement(input => DisbursementRequest { company: input.company.into_inner(), action: Pay::Draft(input.terms) }),
    disbursement_preview(DisbursementSelector) => Disbursement(input => DisbursementRequest { company: input.company.into_inner(), action: Pay::Preview(input.id) }),
    disbursement_approve(DisbursementApproveInput) => Disbursement(input => DisbursementRequest { company: input.company.into_inner(), action: Pay::Approve(input.id, input.destination) }),
    disbursement_revoke(DisbursementSelector) => Disbursement(input => DisbursementRequest { company: input.company.into_inner(), action: Pay::Revoke(input.id) }),
    disbursement_reject(DisbursementRejectInput) => Disbursement(input => DisbursementRequest { company: input.company.into_inner(), action: Pay::Reject(input.id, input.reason) }),
    disbursement_dispatch(DisbursementSelector) => Disbursement(input => DisbursementRequest { company: input.company.into_inner(), action: Pay::Dispatch(input.id) }),
    disbursement_reconcile(DisbursementReconcileInput) => Disbursement(input => DisbursementRequest { company: input.company.into_inner(), action: Pay::Reconcile(input.id, input.date.into()) }),
    disbursement_show(DisbursementSelector) => Disbursement(input => DisbursementRequest { company: input.company.into_inner(), action: Pay::Show(input.id) }),
    disbursement_list(CompanyScope) => Disbursement(input => DisbursementRequest { company: input.company.into_inner(), action: Pay::List }),
    document_draft(DocumentDraftInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Mutation(DocChange::Draft(input.terms)))),
    document_amend(DocumentAmendInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Mutation(DocChange::Amend(input.id, input.revision, input.terms)))),
    document_issue(DocumentIssueInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Mutation(DocChange::Issue(input.id, input.date.into_inner().date())))),
    document_show(DocumentSelector) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Find(input.id))),
    document_print(DocumentSelector) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Print(input.id))),
    document_list(DocumentAsOfInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::List(input.as_of.map(|date| date.into_inner().date()).into()))),
    document_allocations(DocumentAsOfInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Allocations(input.as_of.map(|date| date.into_inner().date()).into()))),
    document_aging(DocumentAgingInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Aging(input.as_of.map(|date| date.into_inner().date()).into(), input.kind))),
    document_allocate(DocumentAllocateInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Mutation(DocChange::Allocate(input.source, input.date.into_inner().date(), input.allocations)))),
    document_credit(DocumentCreditInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Mutation(DocChange::Credit(input.credit)))),
    document_revoke(DocumentRevokeInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Mutation(DocChange::Revoke(input.id, input.date.into_inner().date())))),
    document_proposals(CompanyScope) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Reviews)),
    document_approve(DocumentApprovalInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Review(input.id, ApprovalAction::Approve))),
    document_reject(DocumentRejectionInput) => Document(input => DocumentRequest::new(input.company.into_inner(), Doc::Review(input.id, ApprovalAction::Reject(input.reason)))),
    project_create(ProjectCreateInput) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Create { terms: input.terms })),
    project_archive(ProjectSelector) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Archive { id: input.id })),
    project_list(ProjectListInput) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::List { include_archived: input.include_archived })),
    project_show(ProjectSelector) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Show { id: input.id })),
    project_record(ProjectWorkInput) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Record { work: input.work })),
    project_approve(ProjectApprovalInput) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Approve { id: input.id, date: input.date.into_inner().date().into() })),
    project_time(ProjectSelector) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Time { id: input.id })),
    project_split(ProjectSplitInput) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Split { entry: input.entry, date: input.date.into_inner().date().into(), splits: Splits(input.splits) })),
    project_budget(ProjectBudgetArgs) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Budget { budget: input.budget })),
    project_unbilled(ProjectUnbilledInput) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Unbilled { id: input.id, from: input.from.into_inner().date().into(), through: input.through.into_inner().date().into() })),
    project_draft_invoice(ProjectInvoiceArgs) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::DraftInvoice { invoice: input.invoice })),
    project_cancel_invoice(DocumentSelector) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::CancelInvoice { id: input.id })),
    project_report(ProjectReportInput) => Project(input => ProjectRequest::new(input.company.into_inner(), ProjectCommand::Report { month: input.month, currency: input.currency, detail: input.detail })),
    counterparty_create(CounterpartyCreate) => Counterparty(input => CounterpartyRequest::new(input.company.into_inner(), Party::Create(ContactDetails {
        name: input.name, role: input.role, email: input.email, address: input.address, payment_terms: input.payment_terms,
    }))),
    counterparty_amend(CounterpartyAmend) => Counterparty(input => CounterpartyRequest::new(input.company.into_inner(), Party::Amend(input.id, input.patch))),
    counterparty_archive(CounterpartySelector) => Counterparty(input => CounterpartyRequest::new(input.company.into_inner(), Party::Archive(input.id))),
    counterparty_show(CounterpartySelector) => Counterparty(input => CounterpartyRequest::new(input.company.into_inner(), Party::Show(input.id))),
    counterparty_associate(CounterpartyAssociate) => Counterparty(input => CounterpartyRequest::new(input.company.into_inner(), Party::Associate(input.id, input.transaction_id))),
    counterparty_list(CounterpartyList) => Counterparty(input => CounterpartyRequest::new(input.company.into_inner(), Party::List(
        input.search, input.include_archived, PartyPageRequest { limit: input.limit, offset: input.offset },
    ))),
    counterparty_suggest(CounterpartySuggest) => Counterparty(input => CounterpartyRequest::new(input.company.into_inner(), Party::Suggest(
        input.label, PartyPageRequest { limit: input.limit, offset: input.offset },
    ))),
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
    payroll_fund(PayrollFund) => Payroll(input => input.into()),
    payroll_show(PayrollShow) => Payroll(input => input.into()),
    period_close(PeriodClose) => Period(input => input.into()),
    reconcile_list(ReconcileList) => Reconcile(input => input.into()),
    period_list(CompanyScope) => Period(input => PeriodRequest::List(input.company.into_inner())),
    restore(Restore) => Restore(input => input.into()),
    restore_undo(RestoreUndo) => Restore(input => input.into()),
    audit_log(AuditLog) => AuditLog(input => input.into()),
    audit_undo(AuditUndo) => AuditReceipts(input => ReceiptRequest::Undo(input.company.into_inner(), input.receipt)),
}

try_convert! {
    fixed_asset_register(FixedAssetRegister) => FixedAsset(input => input.try_into()?),
    expense_claim_receipt_intake(ExpenseIntakeInput) => Expense(input => input.try_into()?),
    payroll_preview(PayrollPreview) => Payroll(input => input.try_into()?),
    payroll_import(PayrollImport) => Payroll(input => input.try_into()?),
    txn_link(TxnLink) => Transaction(input => input.try_into()?),
    reconcile_statement(ReconcileStatement) => Reconcile(input => input.try_into()?),
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
    txn_import(TxnImport) => JournalImport(input => JournalImportRequest::new(
        input.company.into_inner(),
        input.transactions,
        CurrencySetting::from(input.currency).try_into()?,
        input.on_conflict.map_or(crate::db::transactions::ConflictStrategy::Skip, Into::into),
        input.dry_run,
    )?),
    txn_propose(TxnPost) => Transaction(input => TxnRequest::propose_mcp(input)?),
    txn_meta_set(TxnMetaSet) => Transaction(input => input.try_into()?),
    txn_meta_unset(TxnMetaUnset) => Transaction(input => input.try_into()?),
    txn_list(TxnList) => Transaction(input => input.try_into()?),
    txn_clear(ClearInput) => Transaction(input => input.try_into()?),
    report(Report) => Report(input => input.try_into()?),
    group_income_statement(GroupIncomeStatement) => GroupReport(input => GroupReportRequest::income_statement(
        input.members.into_iter().map(Text::into_inner).collect(), input.currency.into_inner(), input.from.into_inner().date(), input.to.into_inner().date(),
    )?),
    group_balance_sheet(GroupBalanceSheet) => GroupReport(input => GroupReportRequest::balance_sheet(
        input.members.into_iter().map(Text::into_inner).collect(), input.currency.into_inner(), input.as_of.into_inner().date(),
    )?),
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
        LedgerResponse::FixedAsset(response) => (encode(response.data())?, None),
        LedgerResponse::Expense(response) => (encode(response.data())?, None),
        LedgerResponse::Inventory(response) => (encode(response.data())?, None),
        LedgerResponse::Fx(response) => (encode(response)?, None),
        LedgerResponse::AuditLog(response) => {
            (encode(response.data())?, Some(response.pagination()))
        }
        LedgerResponse::AuditReceipts(response) => (encode(response)?, None),
        LedgerResponse::Budget(response) => (encode(response)?, None),
        LedgerResponse::PaymentCollection(response) => (encode(response)?, None),
        LedgerResponse::Company(response) => (encode(response)?, None),
        LedgerResponse::ContractorTax(_, _, response) => (encode(response)?, None),
        LedgerResponse::Counterparty(_, _, response) => (encode(response)?, None),
        LedgerResponse::Tax(_, _, response) => (encode(response)?, None),
        LedgerResponse::Document(_, _, response) => (encode(response)?, None),
        LedgerResponse::Project(_, _, response) => (encode(response)?, None),
        LedgerResponse::Disbursement(_, _, response) => (encode(response)?, None),
        LedgerResponse::Investment(_, _, response) => (encode(response)?, None),
        LedgerResponse::Payroll(response) => (encode(response)?, None),
        LedgerResponse::Period(_, _, response) => (encode(response)?, None),
        LedgerResponse::Reconcile(_, _, response) => (encode(response)?, None),
        LedgerResponse::Report(response) => (response.data()?, None),
        LedgerResponse::GroupReport(response) => (encode(response)?, None),
        LedgerResponse::Restore(_, response) => (encode(response)?, None),
        LedgerResponse::BankFeed(_, _, response) => (encode(response)?, None),
        LedgerResponse::Rule(_, _, response) => (encode(response)?, None),
        LedgerResponse::Standing(_, _, response) => (encode(response)?, None),
        LedgerResponse::Setup(_, _, response) => (encode(response)?, None),
        LedgerResponse::Summary(response) => (encode(response.envelope(actor))?, None),
        LedgerResponse::Transaction(response) => {
            response.check_reconciliation()?;
            (encode(response)?, response.pagination())
        }
        LedgerResponse::JournalImport(_, response) => (encode(response)?, None),
        LedgerResponse::Verify(response) => (encode(response)?, None),
    };
    Ok((data, pagination.map(encode).transpose()?))
}
