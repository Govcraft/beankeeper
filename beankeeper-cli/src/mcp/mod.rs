//! Transport-independent MCP tools with stdio and authenticated HTTP servers.
//!
//! Tool bodies use the same in-process database operations and JSON renderers
//! as the CLI. Only the transport writes to stdout.

mod dispatch;
mod host;
mod http;
mod input_values;
mod inputs;
mod report_input;

pub use host::{HostTool, HostToolEffect, HostToolHandler};
pub use http::{http_router, http_router_with_host_tools, http_router_with_passphrase, serve_http};
/// Transport options for the HTTP router factories, including host rules and cancellation.
pub use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;

use crate::operation::Operation;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;

use crate::capability::Capability;
use crate::cli::Cli;
use crate::db::{
    Actor,
    audit::ConfiguredActor,
    connection::{Db, ReadConn},
};
use crate::error::CliError;
use dispatch::Request;

/// A ledger tool router, independent of the transport used to serve it.
#[derive(Clone)]
pub struct LedgerServer {
    db: Arc<Mutex<Db>>,
    tool_router: ToolRouter<Self>,
    credentials: Credentials,
    configured_actor: Option<ConfiguredActor>,
}

/// Access ceiling for the tools registered on a ledger server.
#[derive(Clone, Copy)]
pub enum LedgerAccess {
    /// Register read tools only.
    ReadOnly,
    /// Register both read and mutation tools.
    ReadWrite,
}

impl From<bool> for LedgerAccess {
    fn from(read_only: bool) -> Self {
        if read_only {
            Self::ReadOnly
        } else {
            Self::ReadWrite
        }
    }
}

#[derive(Clone)]
enum Credentials {
    Stdio(Option<SecretString>),
    Http,
}

enum CallCredential {
    Stdio(Option<SecretString>),
    Http(Arc<crate::identity::VerifiedAgent>),
}

impl LedgerServer {
    /// Construct a router over an already opened database.
    ///
    /// Uses `BK_ACTOR` for the configured audit label. Read-only routers do not
    /// register mutation tools.
    #[must_use]
    pub fn new(db: Db, access: impl Into<LedgerAccess>) -> Self {
        Self::with_credentials(
            db,
            access.into(),
            Credentials::Stdio(None),
            ConfiguredActor::resolve(None),
        )
    }

    fn with_credentials(
        db: Db,
        access: LedgerAccess,
        credentials: Credentials,
        configured_actor: Option<ConfiguredActor>,
    ) -> Self {
        let mut tool_router = Self::tool_router();
        if matches!(access, LedgerAccess::ReadOnly) {
            for tool in tool_router.list_all() {
                if Capability::for_operation(&tool.name).ok() != Some(Capability::Read) {
                    tool_router.remove_route(&tool.name);
                }
            }
        }
        Self {
            db: Arc::new(Mutex::new(db)),
            tool_router,
            credentials,
            configured_actor,
        }
    }

    async fn execute(
        &self,
        request: Request,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let operation = request.operation();
        let writes_ledger = request.writes_ledger();
        let client_info = context.client_info();
        let configured_actor = self.configured_actor.clone();
        let db = self.db.clone();
        let credential = match self.call_credential(&context) {
            Ok(credential) => credential,
            Err(error) => return tool_error(&error),
        };
        let result = tokio::task::spawn_blocking(move || {
            let db = db
                .lock()
                .map_err(|_| CliError::Database("ledger connection is unavailable".into()))?;
            // Authorize in the same snapshot that will execute the operation.
            let execute = |reader: ReadConn<'_>| {
                let identity = match credential {
                    CallCredential::Stdio(token) => crate::identity::authorize_known_operation(
                        reader,
                        token.as_ref().map(ExposeSecret::expose_secret),
                        operation,
                    )?,
                    CallCredential::Http(identity) => {
                        Some(crate::identity::authorize_verified_operation(
                            reader,
                            Arc::unwrap_or_clone(identity),
                            operation,
                        )?)
                    }
                };
                let actor = Actor::for_mcp(
                    configured_actor.as_ref(),
                    identity,
                    client_info.as_ref().map(|info| info.name.as_str()),
                );
                request.execute(&db, &actor)
            };
            if writes_ledger {
                db.in_write_batch(|| execute(db.read()))
            } else {
                db.read_snapshot(execute)
            }
        })
        .await;
        match result {
            Ok(Ok((data, pagination))) => {
                let mut result = CallToolResult::structured(data);
                if let Some(pagination) = pagination {
                    result.meta = Some(rmcp::model::MetaObject(serde_json::Map::from_iter([(
                        "pagination".into(),
                        pagination,
                    )])));
                }
                result
            }
            Ok(Err(error)) => tool_error(&error),
            Err(_) => tool_error(&CliError::General(
                "ledger operation did not complete".into(),
            )),
        }
    }

    fn call_credential(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallCredential, CliError> {
        match &self.credentials {
            Credentials::Stdio(token) => Ok(CallCredential::Stdio(token.clone())),
            Credentials::Http => Ok(CallCredential::Http(http_identity(context)?.clone())),
        }
    }
}

fn http_identity(
    context: &RequestContext<RoleServer>,
) -> Result<&Arc<crate::identity::VerifiedAgent>, CliError> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<http::RequestAgent>())
        .map(|agent| &agent.0)
        .ok_or_else(|| CliError::Unauthorized("HTTP requires an agent token".into()))
}

fn tool_error(error: &CliError) -> CallToolResult {
    CallToolResult::structured_error(json!({"code":error.code(), "message":error.to_string()}))
}

fn summary_error(company: Option<&str>, error: Option<&serde_json::Value>) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "ok": false,
        "meta": crate::output::json::meta(Operation::Summary, company),
        "error": error,
    }))
}

#[tool_router]
impl LedgerServer {
    #[tool(
        description = "Answer how the owner is doing: read-only financial summary, period comparisons, spending movers, attention counts and totals, brief highlights and optional Desktop targets. Returns the same full JSON envelope as bk summary; all amounts are in one currency."
    )]
    async fn financial_summary(
        &self,
        Parameters(input): Parameters<inputs::FinancialSummaryInput>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let input = match input.0 {
            Ok(input) => input,
            Err(error) => {
                let validation = CliError::Validation(error.cause.to_string());
                return summary_error(
                    error.company.as_deref(),
                    tool_error(&validation).structured_content.as_ref(),
                );
            }
        };
        let company = input.company.to_string();
        let result = self.execute(Request::Summary(input), context).await;
        if result.is_error == Some(true) {
            summary_error(Some(&company), result.structured_content.as_ref())
        } else {
            result
        }
    }

    #[tool(description = "Create a company-scoped security with a fixed quotation currency.")]
    async fn security_create(
        &self,
        Parameters(input): Parameters<inputs::SecurityCreate>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::SecurityCreate(input), context).await
    }

    #[tool(description = "List company securities.")]
    async fn security_list(
        &self,
        Parameters(input): Parameters<inputs::CompanyScope>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::SecurityList(input), context).await
    }

    #[tool(description = "Record a dated price per whole share in major currency units.")]
    async fn security_price(
        &self,
        Parameters(input): Parameters<inputs::SecurityPrice>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::SecurityPrice(input), context).await
    }

    #[tool(
        description = "Acquire an exact share quantity and post total acquisition cost against cash. A duplicate reference fails atomically."
    )]
    async fn investment_buy(
        &self,
        Parameters(input): Parameters<inputs::InvestmentBuy>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::InvestmentBuy(input), context).await
    }

    #[tool(
        description = "Sell shares with FIFO by default, LIFO, or one specific lot; post proceeds, basis relief and realized gain or loss atomically."
    )]
    async fn investment_sell(
        &self,
        Parameters(input): Parameters<inputs::InvestmentSell>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::InvestmentSell(input), context).await
    }

    #[tool(description = "List acquisition lots with remaining share quantities and cost basis.")]
    async fn investment_lots(
        &self,
        Parameters(input): Parameters<inputs::InvestmentLots>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::InvestmentLots(input), context).await
    }

    #[tool(
        description = "Value holdings at the latest price on or before as_of without posting unrealized gains. Monetary results use minor units."
    )]
    async fn investment_holdings(
        &self,
        Parameters(input): Parameters<inputs::InvestmentHoldings>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::InvestmentHoldings(input), context)
            .await
    }

    #[tool(description = "Read setup counts and owner-key readiness for the selected ledger.")]
    async fn ledger_setup_status(
        &self,
        Parameters(input): Parameters<inputs::Empty>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupStatus(input), context)
            .await
    }

    #[tool(
        description = "Read one book's current profile, fiscal-year start, currency, and accounts."
    )]
    async fn ledger_setup_book_show(
        &self,
        Parameters(input): Parameters<inputs::CompanyScope>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupBookShow(input), context)
            .await
    }

    #[tool(
        description = "Freeze bounded CSV or OFX statement contents for owner review in an active setup generation."
    )]
    async fn ledger_setup_statement_preview(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupStatementPreview>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupStatementPreview(input), context)
            .await
    }

    #[tool(
        description = "Commit the exact reviewed statement batch with a live ledger/approve credential."
    )]
    async fn ledger_setup_statement_commit(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupStatementCommit>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupStatementCommit(input), context)
            .await
    }

    #[tool(
        description = "Read the imported and preexisting transaction IDs for a committed statement batch."
    )]
    async fn ledger_setup_statement_receipt(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupStatementReceipt>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupStatementReceipt(input), context)
            .await
    }

    #[tool(
        description = "Issue a short-lived setup-only credential for the encrypted ledger. Requires a live owner Admin credential."
    )]
    async fn ledger_setup_begin(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupBegin>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupBegin(input), context)
            .await
    }

    #[tool(
        description = "Finish a setup generation and revoke its credential. Requires a live owner Admin credential."
    )]
    async fn ledger_setup_finish(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupEnd>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupFinish(input), context)
            .await
    }

    #[tool(
        description = "Revoke a setup generation and its credential. Requires a live owner Admin credential."
    )]
    async fn ledger_setup_revoke(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupEnd>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupRevoke(input), context)
            .await
    }

    #[tool(description = "Create a personal or business book in an active setup generation.")]
    async fn ledger_setup_book_create(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupBookCreate>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupBookCreate(input), context)
            .await
    }

    #[tool(
        description = "Record the first profile for an existing book in an active setup generation."
    )]
    async fn ledger_setup_book_initialize(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupBookInitialize>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupBookInitialize(input), context)
            .await
    }

    #[tool(description = "Supersede a book profile during an active setup generation.")]
    async fn ledger_setup_book_update(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupBookUpdate>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupBookUpdate(input), context)
            .await
    }

    #[tool(
        description = "Apply a versioned chart template to a setup book and return the created accounts."
    )]
    async fn ledger_setup_chart_apply(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupChartApply>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupChartApply(input), context)
            .await
    }

    #[tool(description = "Add the owner's named account to a setup book.")]
    async fn ledger_setup_account_add(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupAccountAdd>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupAccountAdd(input), context)
            .await
    }

    #[tool(
        description = "Validate and preview a balanced opening journal for an active setup book."
    )]
    async fn ledger_setup_opening_preview(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupOpeningPreview>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupOpeningPreview(input), context)
            .await
    }

    #[tool(
        description = "Reserve the exact previewed opening journal for the existing approval workflow."
    )]
    async fn ledger_setup_opening_reserve(
        &self,
        Parameters(input): Parameters<inputs::LedgerSetupOpeningReserve>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::LedgerSetupOpeningReserve(input), context)
            .await
    }

    #[tool(description = "List companies in the ledger.")]
    async fn company_list(
        &self,
        Parameters(input): Parameters<inputs::Empty>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::CompanyList(input), context).await
    }

    #[tool(description = "Show one company by slug.")]
    async fn company_show(
        &self,
        Parameters(input): Parameters<inputs::CompanyShow>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::CompanyShow(input), context).await
    }

    #[tool(
        description = "List accounts, optionally filtering by type or name and including balances."
    )]
    async fn account_list(
        &self,
        Parameters(input): Parameters<inputs::AccountList>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::AccountList(input), context).await
    }

    #[tool(description = "Show one account in a company.")]
    async fn account_show(
        &self,
        Parameters(input): Parameters<inputs::AccountShow>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::AccountShow(input), context).await
    }

    #[tool(description = "Create an account in a company.")]
    async fn account_create(
        &self,
        Parameters(input): Parameters<inputs::AccountCreate>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::AccountCreate(input), context).await
    }

    #[tool(
        description = "Post balanced debits and credits in decimal major units. Always pass reference and on_conflict=skip for safe retries. Conflict strategies are error or skip. Use txn_meta_set/unset to annotate existing postings."
    )]
    async fn txn_post(
        &self,
        Parameters(input): Parameters<inputs::TxnPost>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnPost(input), context).await
    }

    #[tool(
        description = "Propose balanced debits and credits for approval without posting them. Uses the same major-unit amounts and posting flags as txn_post. Requires a verified proposer identity and ledger/propose when enforcement is enabled. Pending references are reserved."
    )]
    async fn txn_propose(
        &self,
        Parameters(input): Parameters<inputs::TxnPost>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnPropose(input), context).await
    }

    #[tool(
        description = "List visible pending transaction proposals. Under enforcement, proposers see their own; approvers and administrators see all. include_rejected also includes approved, rejected and withdrawn proposals."
    )]
    async fn txn_proposals(
        &self,
        Parameters(input): Parameters<inputs::TxnProposals>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnProposals(input), context).await
    }

    #[tool(
        description = "Approve a pending proposal and atomically post it through the audited transaction path. Requires ledger/approve when enforcement is enabled. Approval can exceed the proposer's spend limits and is attributed to the approver."
    )]
    async fn txn_approve(
        &self,
        Parameters(input): Parameters<inputs::TxnApprove>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnApprove(input), context).await
    }

    #[tool(
        description = "Reject a pending proposal with an optional reason. ledger/propose can withdraw the caller's own proposal; ledger/approve can reject any proposal. Resolved proposals cannot be resolved again."
    )]
    async fn txn_reject(
        &self,
        Parameters(input): Parameters<inputs::TxnReject>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnReject(input), context).await
    }

    #[tool(
        description = "Reverse a transaction with a linked mirror posting. Pass the same explicit reference on retries to return the existing reversal. Reconciled transactions cannot be reversed; existing clearings stay on the original."
    )]
    async fn txn_reverse(
        &self,
        Parameters(input): Parameters<inputs::TxnReverse>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnReverse(input), context).await
    }

    #[tool(
        description = "Add or replace transaction annotations as key=value strings. Provide company and exactly one of id or reference. Relationship keys are reserved. No-op retries write no audit row."
    )]
    async fn txn_meta_set(
        &self,
        Parameters(input): Parameters<inputs::TxnMetaSet>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnMetaSet(input), context).await
    }

    #[tool(
        description = "Remove transaction annotation keys. Provide company and exactly one of id or reference. Relationship keys are reserved; absent keys are a no-op."
    )]
    async fn txn_meta_unset(
        &self,
        Parameters(input): Parameters<inputs::TxnMetaUnset>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnMetaUnset(input), context).await
    }

    #[tool(description = "List or count transactions using the CLI filters and pagination.")]
    async fn txn_list(
        &self,
        Parameters(input): Parameters<inputs::TxnList>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnList(input), context).await
    }

    #[tool(description = "Show transaction entries, attachments, and clearing details.")]
    async fn txn_show(
        &self,
        Parameters(input): Parameters<inputs::TxnShow>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnShow(input), context).await
    }

    #[tool(
        description = "Run a financial report. Select kind and that report's options; results match the CLI JSON data payload."
    )]
    async fn report(
        &self,
        Parameters(input): Parameters<inputs::Report>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::Report(input), context).await
    }

    #[tool(description = "List budget entries for a company and year.")]
    async fn budget_list(
        &self,
        Parameters(input): Parameters<inputs::BudgetList>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::BudgetList(input), context).await
    }

    #[tool(
        description = "Set a monthly amount or distribute an annual budget across twelve months."
    )]
    async fn budget_set(
        &self,
        Parameters(input): Parameters<inputs::BudgetSet>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::BudgetSet(input), context).await
    }

    #[tool(description = "Delete a monthly or annual budget; this call confirms deletion.")]
    async fn budget_delete(
        &self,
        Parameters(input): Parameters<inputs::BudgetDelete>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::BudgetDelete(input), context).await
    }

    #[tool(
        description = "Clear or reconcile entries by transaction and entry IDs, reference and account, or an atomic in-memory batch."
    )]
    async fn txn_clear(
        &self,
        Parameters(input): Parameters<inputs::ClearInput>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnClear(input), context).await
    }

    #[tool(
        description = "Find orphaned intercompany correlations across the ledger without changing it."
    )]
    async fn txn_reconcile(
        &self,
        Parameters(input): Parameters<inputs::Empty>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::TxnReconcile(input), context).await
    }

    #[tool(
        description = "Close a company through an inclusive date, rejecting new postings on or before it. Requires ledger/admin when enforcement is on."
    )]
    async fn period_close(
        &self,
        Parameters(input): Parameters<inputs::PeriodClose>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::PeriodClose(input), context).await
    }

    #[tool(
        description = "Explicitly reopen all closed dates for a company with a required reason. Requires ledger/admin when enforcement is on."
    )]
    async fn period_reopen(
        &self,
        Parameters(input): Parameters<inputs::PeriodReopen>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::PeriodReopen(input), context).await
    }

    #[tool(
        description = "List a company's current posting cutoff and last close/reopen attribution."
    )]
    async fn period_list(
        &self,
        Parameters(input): Parameters<inputs::CompanyScope>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::PeriodList(input), context).await
    }

    #[tool(
        description = "Verify every company in the ledger and return balance, audit-chain, and check evidence."
    )]
    async fn verify(
        &self,
        Parameters(input): Parameters<inputs::Empty>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::Verify(input), context).await
    }

    #[tool(
        description = "List audit changes, optionally filtered by actor, verified agent URI, company, entity, or action."
    )]
    async fn audit_log(
        &self,
        Parameters(input): Parameters<inputs::AuditLog>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute(Request::AuditLog(input), context).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LedgerServer {
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let mut tools = self.tool_router.list_all();
        if matches!(self.credentials, Credentials::Http) {
            let identity = http_identity(&context)
                .map_err(|error| http::protocol_error(&error))?
                .clone();
            let db = self.db.clone();
            tools = tokio::task::spawn_blocking(move || {
                let db = db
                    .lock()
                    .map_err(|_| CliError::Database("ledger connection is unavailable".into()))?;
                db.read_snapshot(|reader| {
                    identity.check_live(reader)?;
                    if identity.enforces_permissions(crate::db::policy::get(reader)?.enforce) {
                        let mut allowed = Vec::new();
                        for tool in tools {
                            if identity.allows_known_operation(tool.name.parse()?)? {
                                allowed.push(tool);
                            }
                        }
                        return Ok(allowed);
                    }
                    Ok(tools)
                })
            })
            .await
            .map_err(|_| rmcp::ErrorData::internal_error("tool discovery failed", None))?
            .map_err(|error| http::protocol_error(&error))?;
        }
        Ok(rmcp::model::ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("beankeeper", env!("CARGO_PKG_VERSION")))
            .with_instructions("Amounts in inputs use decimal major-unit strings; amounts in results retain integer minor units and add decimal-string _display companions. txn_list and audit_log return pagination in result _meta.pagination. Always pass a reference to txn_post, and on_conflict=skip for safe retries. Direct posts enforce per-agent spend limits. Use txn_propose to request approval when a post exceeds a limit; txn_approve requires its own grant and posts as the approver. Pending proposals are outside the ledger until approved. Company is required for company-scoped tools. Never pass database paths or secrets to tools.")
    }
}

/// Serve MCP JSON-RPC on stdin/stdout until the client disconnects.
///
/// Database selection and secret resolution happen once before starting the
/// transport. A missing database is never created.
///
/// # Errors
///
/// Returns a CLI error for startup, database, passphrase or transport failures.
pub fn serve(cli: &Cli, read_only: bool) -> Result<(), CliError> {
    if cli.passphrase.passphrase_fd == Some(0) {
        return Err(CliError::Usage(
            "MCP uses stdin; choose a passphrase file, command, or a separate file descriptor"
                .into(),
        ));
    }
    let passphrase = crate::passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open_existing(&cli.db, passphrase.as_ref())?;
    drop(passphrase);
    let identity = if let Some(token) = &cli.agent_token {
        Some(crate::identity::verify(db.read(), token.expose_secret())?)
    } else {
        None
    };
    let enforce = crate::db::policy::get(db.read())?.enforce;
    if enforce && identity.is_none() {
        return Err(CliError::Unauthorized(
            "ledger enforcement requires an agent token".into(),
        ));
    }
    let mut server = LedgerServer::with_credentials(
        db,
        read_only.into(),
        Credentials::Stdio(cli.agent_token.clone()),
        ConfiguredActor::resolve(cli.actor.as_deref()),
    );
    if let Some(identity) = identity
        && identity.enforces_permissions(enforce)
    {
        for tool in server.tool_router.list_all() {
            if !identity.allows_known_operation(tool.name.parse()?)? {
                server.tool_router.remove_route(&tool.name);
            }
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let service = server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|error| CliError::General(format!("MCP startup failed: {error}")))?;
        service
            .waiting()
            .await
            .map_err(|error| CliError::General(format!("MCP transport failed: {error}")))?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use crate::db::CreateAccountParams;
    use crate::identity::{Admin, Granted};

    use super::*;
    use serde_json::Value;

    #[test]
    fn every_mcp_tool_has_an_explicit_requirement() -> Result<(), CliError> {
        let server = LedgerServer::new(Db::open_in_memory()?, LedgerAccess::ReadWrite);
        for tool in server.tool_router.list_all() {
            Capability::for_operation(&tool.name)?;
        }
        Ok(())
    }

    #[test]
    fn proposal_tools_respect_read_only_router_and_input_boundaries() -> Result<(), CliError> {
        let server = LedgerServer::new(Db::open_in_memory()?, LedgerAccess::ReadOnly);
        let tools = server.tool_router.list_all();
        assert!(tools.iter().any(|tool| tool.name == "txn_proposals"));
        for name in ["txn_propose", "txn_approve", "txn_reject"] {
            assert!(!tools.iter().any(|tool| tool.name == name));
        }
        for input in [
            json!({"company":"acme", "id":"proposal_123", "agent_token":"secret"}),
            json!({"company":"acme", "id":"proposal_123", "db":"other.db"}),
        ] {
            assert!(serde_json::from_value::<inputs::TxnApprove>(input.clone()).is_err());
            assert!(serde_json::from_value::<inputs::TxnReject>(input).is_err());
        }
        Ok(())
    }

    #[test]
    fn investment_read_only_router_omits_mutations() -> Result<(), CliError> {
        let server = LedgerServer::new(Db::open_in_memory()?, LedgerAccess::ReadOnly);
        let tools = server.tool_router.list_all();
        for name in [
            "security_create",
            "security_price",
            "investment_buy",
            "investment_sell",
        ] {
            assert!(!tools.iter().any(|tool| tool.name == name));
        }
        for name in ["security_list", "investment_lots", "investment_holdings"] {
            assert!(tools.iter().any(|tool| tool.name == name));
        }
        Ok(())
    }

    fn decode<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, CliError> {
        serde_json::from_value(input).map_err(|error| CliError::Usage(error.to_string()))
    }

    #[test]
    fn investment_tools_share_trade_valuation_and_actor_behavior() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture"), |grant| {
            crate::db::companies::create_company(grant, &"acme".parse()?, "Acme", None)
        })?;
        for (code, kind) in [
            ("1000", beankeeper::types::AccountType::Asset),
            ("1200", beankeeper::types::AccountType::Asset),
            ("4000", beankeeper::types::AccountType::Revenue),
        ] {
            Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture"), |grant| {
                crate::db::accounts::create_account(
                    grant,
                    &CreateAccountParams {
                        company_slug: &"acme".parse()?,
                        code,
                        name: code,
                        account_type: kind,
                        default_tax_category: None,
                    },
                )
            })?;
        }
        let actor = Actor::for_mcp(None, None, Some("Investment Client"));
        let call = |tool, mut input: Value| {
            input["company"] = json!("acme");
            let request = match tool {
                "security_create" => Request::SecurityCreate(decode(input)?),
                "security_list" => Request::SecurityList(decode(input)?),
                "investment_buy" => Request::InvestmentBuy(decode(input)?),
                "security_price" => Request::SecurityPrice(decode(input)?),
                "investment_sell" => Request::InvestmentSell(decode(input)?),
                "investment_holdings" => Request::InvestmentHoldings(decode(input)?),
                "investment_lots" => Request::InvestmentLots(decode(input)?),
                _ => return Err(CliError::General(format!("unexpected test tool: {tool}"))),
            };
            request.execute(&db, &actor).map(|(data, _)| data)
        };
        call(
            "security_create",
            json!({"symbol":"TEST", "name":"Test security", "currency":"USD"}),
        )?;
        assert_eq!(
            call("security_list", json!({}))?.as_array().map(Vec::len),
            Some(1)
        );
        let bought = call(
            "investment_buy",
            json!({"symbol":"TEST", "quantity":"1.5", "cost":"15.00", "date":"2026-01-01", "asset":"1200", "cash":"1000", "reference":"buy-test"}),
        )?;
        assert_eq!(bought["cost_basis"], 1500);
        call(
            "security_price",
            json!({"symbol":"TEST", "date":"2026-01-02", "price":"12.00"}),
        )?;
        let sold = call(
            "investment_sell",
            json!({"symbol":"TEST", "quantity":"0.5", "proceeds":"7.00", "date":"2026-01-03", "asset":"1200", "cash":"1000", "gain":"4000", "reference":"sell-test"}),
        )?;
        assert_eq!(sold["cost_basis"], 500);
        assert_eq!(sold["realized_gain"], 200);
        let holdings = call("investment_holdings", json!({"as_of":"2026-01-03"}))?;
        assert_eq!(holdings[0]["quantity"], "1");
        assert_eq!(holdings[0]["cost_basis"], 1000);
        assert_eq!(holdings[0]["market_value"], 1200);
        assert_eq!(holdings[0]["unrealized_gain"], 200);
        assert_eq!(
            call("investment_lots", json!({"symbol":"TEST"}))?
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        let posted_by: String = db.read().query_row(
            "SELECT posted_by FROM transactions WHERE reference = 'sell-test'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(posted_by, "mcp:investment-client");
        crate::db::audit::verify_hash_chain(db.read())?;
        crate::db::audit::verify_creations(db.read())?;
        Ok(())
    }

    #[test]
    fn actor_names_are_bounded_and_sanitized() {
        let client_actor = |name| Actor::for_mcp(None, None, name);
        assert_eq!(client_actor(None).name(), "mcp:unknown");
        assert_eq!(client_actor(Some(" /\n ")).name(), "mcp:unknown");
        assert_eq!(
            client_actor(Some("Claude AI/Client\n")).name(),
            "mcp:claude-ai-client"
        );
        assert_eq!(client_actor(Some(&"x".repeat(100))).name().len(), 84);
    }

    #[test]
    fn report_inputs_cover_all_kinds_and_keep_cli_defaults() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture"), |grant| {
            crate::db::companies::create_company(grant, &"acme".parse()?, "Acme", None)
        })?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture"), |grant| {
            crate::db::accounts::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None,
                },
            )
        })?;
        for input in [
            json!({"kind":"trial-balance", "account_type":"asset"}),
            json!({"kind":"balance", "account":"1000"}),
            json!({"kind":"income-statement"}),
            json!({"kind":"balance-sheet"}),
            json!({"kind":"trend", "metric":"spending"}),
            json!({"kind":"trend", "metric":"net-worth", "period":"quarter"}),
            json!({"kind":"tax-summary"}),
            json!({"kind":"schedule-c"}),
            json!({"kind":"1099"}),
            json!({"kind":"1099-nec", "threshold":"0"}),
            json!({"kind":"ledger", "account":["1000"]}),
            json!({"kind":"budget-variance", "year":2026, "account_type":"asset"}),
            json!({"kind":"cash-flow", "cash_account":["1000"]}),
            json!({"kind":"aging", "account":["1000"]}),
            json!({"kind":"reconciliation", "account":"1000"}),
        ] {
            let mut input = input;
            input["company"] = json!("acme");
            let input: inputs::Report = serde_json::from_value(input)
                .map_err(|error| CliError::General(error.to_string()))?;
            Request::Report(input).execute(&db, &Actor::for_mcp(None, None, None))?;
        }
        Ok(())
    }

    #[test]
    fn startup_options_cannot_be_supplied_as_tool_inputs() {
        assert!(
            serde_json::from_value::<inputs::TxnList>(json!({"company":"acme", "db":"other.db"}))
                .is_err()
        );
        assert!(serde_json::from_value::<inputs::TxnPost>(json!({"company":"acme", "description":"test", "debits":[], "credits":[], "passphrase":"secret"})).is_err());
        assert!(
            serde_json::from_value::<inputs::Report>(
                json!({"company":"acme", "kind":"balance-sheet", "db":"other.db"})
            )
            .is_err()
        );
    }
}
