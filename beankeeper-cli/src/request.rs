//! Transport-independent ledger requests and their typed results.
//!
//! This library owns the open ledger and its audited storage. CLI, MCP, and
//! embedded hosts can feed validated requests directly to the same executor.

mod calendar_date;
mod collection;
mod currency;
mod local_date;
pub(crate) mod text;

pub use calendar_date::{CalendarDateArg, CalendarDateError};
pub use collection::{EmptyCollection, NonEmpty};
pub(crate) use currency::CurrencySetting;
pub use local_date::LocalDate;

use crate::{
    cli::Cli,
    commands::expense_claim::{ExpenseRequest, ExpenseResponse},
    commands::inventory::{InventoryRequest, InventoryResponse},
    commands::{
        account::{AccountRequest, AccountResponse},
        audit::{AuditQuery, AuditResponse, ReceiptRequest, ReceiptResponse},
        bank_feed::{BankFeedRequest, BankFeedResponse},
        budget::{BudgetRequest, BudgetResponse},
        collection::{PaymentCollectionRequest, PaymentCollectionResponse},
        company::{CompanyRequest, CompanyResponse},
        contractor_tax::{ContractorTaxRequest, ContractorTaxResponse},
        counterparty::{CounterpartyRequest, CounterpartyResponse},
        disbursement::{DisbursementRequest, Response as DisbursementResponse},
        document::{DocumentRequest, DocumentResponse},
        fixed_asset::{FixedAssetRequest, FixedAssetResponse},
        fx::{FxRequest, FxResponse},
        group_report::{GroupReportRequest, GroupReportResponse},
        investment::{InvestmentRequest, InvestmentResponse},
        payroll::{PayrollRequest, PayrollResponse},
        period::{PeriodRequest, PeriodResponse},
        project::{ProjectRequest, ProjectResponse},
        reconcile::{ReconcileRequest, ReconcileResponse},
        report::{ReportRequest, ReportResponse},
        rule::{RuleRequest, RuleResponse},
        setup::{SetupRequest, SetupResponse},
        standing::{StandingRequest, StandingResponse},
        summary::{SummaryRequest, SummaryResponse},
        tax::{TaxRequest, TaxResponse},
        txn::{TxnRequest, TxnResponse},
        txn_import::{self, BatchResult, JournalImportRequest},
    },
    db::{
        Actor, Db,
        audit::restore::{RestoreReceipt, RestoreRequest},
        companies::CompanySlug,
        connection::ReadConn,
        verification::VerifiedLedger,
    },
    error::CliError,
    operation::Operation,
    output::json::Meta,
};

/// A validated operation against an open ledger.
#[non_exhaustive]
pub enum LedgerRequest {
    Account(AccountRequest),
    Fx(FxRequest),
    FixedAsset(FixedAssetRequest),
    AuditLog(AuditQuery),
    AuditReceipts(ReceiptRequest),
    BankFeed((CompanySlug, BankFeedRequest)),
    Budget(BudgetRequest),
    PaymentCollection(PaymentCollectionRequest),
    Company(CompanyRequest),
    Counterparty(CounterpartyRequest),
    ContractorTax(ContractorTaxRequest),
    Document(DocumentRequest),
    Tax(TaxRequest),
    Expense(ExpenseRequest),
    Project(ProjectRequest),
    Inventory(InventoryRequest),
    Disbursement(DisbursementRequest),
    Investment(InvestmentRequest),
    Payroll(PayrollRequest),
    Period(PeriodRequest),
    Reconcile(ReconcileRequest),
    Report(ReportRequest),
    GroupReport(GroupReportRequest),
    Rule(RuleRequest),
    Restore(RestoreRequest),
    Setup(SetupRequest),
    Standing(StandingRequest),
    Summary(SummaryRequest),
    Transaction(TxnRequest),
    JournalImport(JournalImportRequest),
    Verify,
}

/// The result of a ledger operation, before transport formatting.
#[non_exhaustive]
pub enum LedgerResponse {
    Account(AccountResponse),
    Fx(FxResponse),
    FixedAsset(FixedAssetResponse),
    AuditLog(AuditResponse),
    AuditReceipts(ReceiptResponse),
    BankFeed(Operation, CompanySlug, BankFeedResponse),
    Budget(BudgetResponse),
    PaymentCollection(PaymentCollectionResponse),
    Company(CompanyResponse),
    Counterparty(Operation, CompanySlug, CounterpartyResponse),
    ContractorTax(Operation, CompanySlug, ContractorTaxResponse),
    Document(Operation, CompanySlug, DocumentResponse),
    Tax(Operation, CompanySlug, TaxResponse),
    Expense(ExpenseResponse),
    Project(Operation, CompanySlug, ProjectResponse),
    Inventory(InventoryResponse),
    Disbursement(Operation, CompanySlug, DisbursementResponse),
    Investment(Operation, CompanySlug, InvestmentResponse),
    Payroll(PayrollResponse),
    Period(Operation, CompanySlug, PeriodResponse),
    Reconcile(Operation, CompanySlug, ReconcileResponse),
    Report(ReportResponse),
    GroupReport(GroupReportResponse),
    Rule(Operation, CompanySlug, RuleResponse),
    Restore(Operation, RestoreReceipt),
    Setup(Operation, Option<CompanySlug>, SetupResponse),
    Standing(Operation, CompanySlug, StandingResponse),
    Summary(SummaryResponse),
    Transaction(TxnResponse),
    JournalImport(CompanySlug, BatchResult),
    Verify(VerifiedLedger),
}

fn authorize_actor<'snapshot>(
    reader: ReadConn<'snapshot>,
    operation: Operation,
    resolve_actor: impl FnOnce(ReadConn<'snapshot>) -> Result<Actor, CliError>,
) -> Result<Actor, CliError> {
    let actor = resolve_actor(reader)?;
    if let Some(identity) = actor.identity() {
        crate::identity::authorize_verified_operation(reader, identity.clone(), operation)?;
    } else {
        crate::identity::authorize_known_operation(reader, None, operation)?;
    }
    Ok(actor)
}

impl LedgerRequest {
    /// The operation used for authorization and discovery.
    #[must_use]
    pub fn operation(&self) -> Operation {
        match self {
            Self::Account(request) => request.operation(),
            Self::Fx(request) => request.operation(),
            Self::FixedAsset(request) => request.operation(),
            Self::AuditLog(_) => Operation::AuditLog,
            Self::AuditReceipts(request) => request.operation(),
            Self::BankFeed((_, request)) => request.operation(),
            Self::Budget(request) => request.operation(),
            Self::PaymentCollection(request) => request.operation(),
            Self::Company(request) => request.operation(),
            Self::Counterparty(request) => request.operation(),
            Self::ContractorTax(request) => request.operation(),
            Self::Document(request) => request.operation(),
            Self::Tax(request) => request.operation(),
            Self::Expense(request) => request.operation(),
            Self::Project(request) => request.operation(),
            Self::Inventory(request) => request.operation(),
            Self::Disbursement(request) => request.operation(),
            Self::Investment(request) => request.operation(),
            Self::Payroll(request) => request.operation(),
            Self::Period(request) => request.operation(),
            Self::Reconcile(request) => request.operation(),
            Self::Report(request) => request.operation(),
            Self::GroupReport(request) => request.operation(),
            Self::Rule(request) => request.operation(),
            Self::Restore(request) => request.operation(),
            Self::Setup(request) => request.operation(),
            Self::Standing(request) => request.operation(),
            Self::Summary(_) => Operation::Summary,
            Self::Transaction(request) => request.operation(),
            Self::JournalImport(_) => Operation::TxnImport,
            Self::Verify => Operation::Verify,
        }
    }

    /// Execute in one authorized read snapshot or write batch.
    ///
    /// # Errors
    /// Returns authorization, ledger-state, or storage errors.
    pub fn execute(self, db: &Db, actor: &Actor) -> Result<LedgerResponse, CliError> {
        self.execute_with_actor(db, |_| Ok(actor.clone()))
            .map(|(response, _)| response)
    }

    pub(crate) fn execute_with_actor(
        self,
        db: &Db,
        resolve_actor: impl for<'snapshot> FnOnce(ReadConn<'snapshot>) -> Result<Actor, CliError>,
    ) -> Result<(LedgerResponse, Actor), CliError> {
        let operation = self.operation();
        if matches!(
            operation,
            Operation::BankFeedSync
                | Operation::DisbursementPreview
                | Operation::DisbursementApprove
                | Operation::DisbursementDispatch
        ) || matches!(&self, Self::PaymentCollection(request) if request.calls_provider())
        {
            // Release the authorization snapshot before provider I/O. Each
            // workflow rechecks the actor in its own audited write stages.
            let actor =
                db.read_snapshot(|reader| authorize_actor(reader, operation, resolve_actor))?;
            return self
                .execute_authorized(db, &actor, operation)
                .map(|response| (response, actor));
        }
        if operation.writes_ledger() {
            db.in_write_batch_for(operation.into(), || {
                self.execute_in_snapshot(db, db.read(), resolve_actor)
            })
        } else {
            db.read_snapshot(|reader| self.execute_in_snapshot(db, reader, resolve_actor))
        }
    }

    fn execute_in_snapshot<'snapshot>(
        self,
        db: &Db,
        reader: ReadConn<'snapshot>,
        resolve_actor: impl FnOnce(ReadConn<'snapshot>) -> Result<Actor, CliError>,
    ) -> Result<(LedgerResponse, Actor), CliError> {
        let operation = self.operation();
        let actor = authorize_actor(reader, operation, resolve_actor)?;
        let response = self.execute_authorized(db, &actor, operation)?;
        Ok((response, actor))
    }

    fn execute_authorized(
        self,
        db: &Db,
        actor: &Actor,
        operation: Operation,
    ) -> Result<LedgerResponse, CliError> {
        match self {
            Self::Account(request) => request.execute(db, actor).map(LedgerResponse::Account),
            Self::Fx(request) => request.execute(db, actor).map(LedgerResponse::Fx),
            Self::FixedAsset(request) => request.execute(db, actor).map(LedgerResponse::FixedAsset),
            Self::AuditLog(request) => request.execute(db).map(LedgerResponse::AuditLog),
            Self::AuditReceipts(request) => request
                .execute(db, actor)
                .map(LedgerResponse::AuditReceipts),
            Self::Budget(request) => request.execute(db, actor).map(LedgerResponse::Budget),
            Self::PaymentCollection(request) => request
                .execute(db, actor)
                .map(LedgerResponse::PaymentCollection),
            Self::Company(request) => request.execute(db, actor).map(LedgerResponse::Company),
            Self::Expense(request) => request.execute(db, actor).map(LedgerResponse::Expense),
            Self::Counterparty(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Counterparty(operation, company, response))
            }
            Self::Tax(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Tax(operation, company, response))
            }
            Self::ContractorTax(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::ContractorTax(operation, company, response))
            }
            Self::Project(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Project(operation, company, response))
            }
            Self::Inventory(request) => request.execute(db, actor).map(LedgerResponse::Inventory),
            Self::Disbursement(request) => {
                let company = request.company.clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Disbursement(operation, company, response))
            }
            Self::Document(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Document(operation, company, response))
            }
            Self::Investment(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Investment(operation, company, response))
            }
            Self::Payroll(request) => request.execute(db, actor).map(LedgerResponse::Payroll),
            Self::Period(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Period(operation, company, response))
            }
            Self::Reconcile(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Reconcile(operation, company, response))
            }
            Self::BankFeed((company, request)) => {
                let response = request.execute(db, db.read(), actor, &company)?;
                Ok(LedgerResponse::BankFeed(operation, company, response))
            }
            Self::Report(request) => request.execute(db).map(LedgerResponse::Report),
            Self::GroupReport(request) => {
                request.execute(db.read()).map(LedgerResponse::GroupReport)
            }
            Self::Restore(request) => request
                .execute(db, actor)
                .map(|result| LedgerResponse::Restore(operation, result)),
            Self::Rule(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Rule(operation, company, response))
            }
            Self::Setup(request) => {
                let company = request.render_company().cloned();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Setup(operation, company, response))
            }
            Self::Standing(request) => {
                let company = request.company().clone();
                let response = request.execute(db, actor)?;
                Ok(LedgerResponse::Standing(operation, company, response))
            }
            Self::Summary(request) => request.execute(db).map(LedgerResponse::Summary),
            Self::Transaction(request) => {
                request.execute(db, actor).map(LedgerResponse::Transaction)
            }
            Self::JournalImport(request) => {
                let company = request.company().clone();
                request
                    .execute(db, actor)
                    .map(|result| LedgerResponse::JournalImport(company, result))
            }
            Self::Verify => VerifiedLedger::read(db).map(LedgerResponse::Verify),
        }
    }
}

impl LedgerResponse {
    pub(crate) fn render(&self, cli: &Cli, meta: Option<Meta>) -> Result<(), CliError> {
        match self {
            Self::Account(response) => response.render(cli),
            Self::Fx(response) => response.render(cli),
            Self::FixedAsset(response) => response.render(cli),
            Self::AuditLog(response) => response.render(cli),
            Self::AuditReceipts(response) => response.render(cli),
            Self::Budget(response) => response.render(cli),
            Self::PaymentCollection(response) => response.render(cli),
            Self::Company(response) => response.render(cli),
            Self::Tax(operation, company, response) => response.render(cli, *operation, company),
            Self::ContractorTax(operation, company, response) => {
                response.render(cli, *operation, company)
            }
            Self::Project(operation, company, response) => {
                response.render(cli, *operation, company)
            }
            Self::Inventory(response) => response.render(cli),
            Self::Disbursement(operation, company, response) => {
                response.render(cli, *operation, company)
            }
            Self::Document(operation, company, response) => {
                response.render(cli, *operation, company)
            }
            Self::Expense(response) => response.render(cli),
            Self::Counterparty(operation, company, response) => {
                response.render(cli, *operation, company)
            }
            Self::Investment(operation, company, response) => {
                response.render(cli, *operation, company.as_str())
            }
            Self::Payroll(response) => response.render(cli),
            Self::Period(operation, company, response) => {
                response.render(cli, *operation, company.as_str())
            }
            Self::Reconcile(operation, company, response) => {
                response.render(cli, *operation, company)
            }
            Self::Report(response) => response.render(cli),
            Self::GroupReport(response) => response.render(cli),
            Self::Restore(operation, receipt) => {
                if cli.is_json() {
                    let meta = cli.meta(*operation, None);
                    println!("{}", crate::output::json::envelope_ok(meta, receipt)?);
                } else if !cli.verbosity.quiet {
                    eprintln!(
                        "{} Undo with `bk restore-undo {}` on this ledger.",
                        receipt.summary(),
                        receipt.restore_id()
                    );
                }
                Ok(())
            }
            Self::BankFeed(operation, company, response) => {
                response.render(cli, *operation, company)
            }
            Self::Rule(operation, company, response) => response.render(cli, *operation, company),
            Self::Setup(operation, company, response) => {
                response.render(cli, *operation, company.as_ref().map(CompanySlug::as_str))
            }
            Self::Standing(operation, company, response) => {
                response.render(cli, *operation, company)
            }
            Self::Summary(response) => response.render(cli),
            Self::Transaction(response) => response.render(cli, meta),
            Self::JournalImport(company, response) => {
                if let Some(rendered) = txn_import::render_result(cli, company.as_str(), response)?
                {
                    println!("{rendered}");
                }
                Ok(())
            }
            Self::Verify(response) => crate::commands::verify::render(response, cli),
        }
    }
}
