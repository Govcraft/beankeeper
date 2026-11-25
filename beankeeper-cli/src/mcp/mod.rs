//! Transport-independent MCP tools with stdio and authenticated HTTP servers.
//!
//! Tool bodies use the same in-process database operations and JSON renderers
//! as the CLI. Only the transport writes to stdout.

pub(crate) mod catalog;
mod demo;
mod dispatch;
mod host;
mod http;
pub(crate) mod input_values;
pub(crate) mod inputs;
mod report_input;
mod setup_investment_input;
mod targets;

pub use demo::DemoLedgerTool;
pub use host::{HostTool, HostToolEffect, HostToolHandler};
pub use http::{
    LEDGER_TARGET_HEADER, http_router, http_router_with_host_tools,
    http_router_with_ledger_targets, http_router_with_passphrase, serve_http,
};
/// Transport options for the HTTP router factories, including host rules and cancellation.
pub use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
pub use targets::{DetachedLedger, HostOwnedTarget, LEDGER_TARGET_META_KEY, LedgerTargets};

use crate::operation::Operation;

use rmcp::handler::server::{router::tool::ToolRouter, tool::ToolCallContext};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, ServiceExt, tool_handler};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;

use crate::cli::Cli;
use crate::db::{
    Actor, ActorLabel,
    connection::{Db, ReadConn},
};
use crate::error::CliError;
pub use crate::ledger_target::{LedgerTarget, NamedLedgerTarget};
use crate::request::LedgerRequest;
use targets::OpenedLedger;

/// A ledger tool router, independent of the transport used to serve it.
#[derive(Clone)]
pub struct LedgerServer {
    tool_router: ToolRouter<Self>,
    backend: Backend,
    configured_actor: Option<ActorLabel>,
    show_client_hint_notice: bool,
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
enum Backend {
    Stdio {
        ledger: OpenedLedger,
        token: Option<SecretString>,
    },
    Http,
}

enum LedgerCall {
    Stdio {
        ledger: OpenedLedger,
        token: Option<SecretString>,
    },
    Http(http::AuthenticatedLedger),
}

impl LedgerCall {
    fn ledger(&self) -> &OpenedLedger {
        match self {
            Self::Stdio { ledger, .. } => ledger,
            Self::Http(call) => call.ledger(),
        }
    }

    fn identity(
        &self,
        reader: ReadConn<'_>,
    ) -> Result<Option<crate::identity::VerifiedAgent>, CliError> {
        match self {
            Self::Stdio { token, .. } => token
                .as_ref()
                .map(|token| crate::identity::verify(reader, token.expose_secret()))
                .transpose(),
            Self::Http(call) => Ok(Some(call.identity().clone())),
        }
    }
}

impl LedgerServer {
    /// Construct a router over an already opened database.
    ///
    /// Uses `BK_ACTOR` for the configured audit label. Read-only routers do not
    /// register mutation tools.
    #[must_use]
    pub fn new(db: Db, access: impl Into<LedgerAccess>) -> Self {
        Self::with_backend(
            access.into(),
            Backend::Stdio {
                ledger: OpenedLedger::new(LedgerTarget::default(), db),
                token: None,
            },
            ActorLabel::configured(None),
        )
    }

    fn with_backend(
        access: LedgerAccess,
        backend: Backend,
        configured_actor: Option<ActorLabel>,
    ) -> Self {
        let tool_router = catalog::router(access);
        Self {
            tool_router,
            backend,
            configured_actor,
            show_client_hint_notice: false,
        }
    }

    async fn execute(
        &self,
        operation: Operation,
        request: Result<LedgerRequest, CliError>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let request = match request {
            Ok(request) => request,
            Err(error) => return tool_error(&error),
        };
        let actual = request.operation();
        if operation != actual
            && actual.exposure() != crate::operation::Exposure::Grouped(operation)
        {
            return tool_error(&CliError::General(
                "tool is registered for a different operation".into(),
            ));
        }
        let show_client_hint_notice = actual.writes_ledger() && self.show_client_hint_notice;
        let client_info = context.client_info();
        let configured_actor = self.configured_actor.clone();
        let call = match self.ledger_call(&context) {
            Ok(call) => call,
            Err(error) => return tool_error(&error),
        };
        let result = tokio::task::spawn_blocking(move || {
            let db = call.ledger().lock()?;
            let (response, actor) = request.execute_with_actor(&db, |reader| {
                Ok(Actor::for_mcp(
                    configured_actor,
                    call.identity(reader)?,
                    client_info.as_ref().map(|info| info.name.as_str()),
                ))
            })?;
            let response = dispatch::response(&response, &actor)?;
            let notice = if show_client_hint_notice {
                actor.unverified_hint_notice()
            } else {
                None
            };
            Ok((response, notice))
        })
        .await;
        match result {
            Ok(Ok(((data, pagination), notice))) => {
                if let Some(notice) = notice {
                    crate::commands::audit::show_hint_notice(&notice);
                }
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

    fn ledger_call(&self, context: &RequestContext<RoleServer>) -> Result<LedgerCall, CliError> {
        match &self.backend {
            Backend::Stdio { ledger, token } => Ok(LedgerCall::Stdio {
                ledger: ledger.clone(),
                token: token.clone(),
            }),
            Backend::Http => Ok(LedgerCall::Http(
                http::authenticated_ledger(context)?.clone(),
            )),
        }
    }

    fn result_target<'a>(
        &'a self,
        context: &'a RequestContext<RoleServer>,
    ) -> Option<&'a NamedLedgerTarget> {
        match &self.backend {
            Backend::Stdio { ledger, .. } => ledger.named(),
            Backend::Http => http::authenticated_ledger(context).ok()?.ledger().named(),
        }
    }
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

impl LedgerServer {
    async fn execute_summary(
        &self,
        input: inputs::FinancialSummary,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let company = input.company.to_string();
        let result = self
            .execute(
                Operation::Summary,
                dispatch::financial_summary(input),
                context,
            )
            .await;
        if result.is_error == Some(true) {
            summary_error(Some(&company), result.structured_content.as_ref())
        } else {
            result
        }
    }
}

const SERVER_INSTRUCTIONS: &str = "Amounts in inputs use decimal major-unit strings. Results retain integer minor units and add decimal-string _display companions. Results with pagination include it in _meta.pagination. Use a stable reference and on_conflict=skip for safe posting retries. Tell the owner what changed and give them the receipt or transaction ID for an audited correction. Use the correction tools listed for your credential. Describe a proposal as pending until an approver applies it. Use company for company-scoped tools. Never pass database paths or secrets to tools.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LedgerServer {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let target = self.result_target(&context).cloned();
        let mut response = self
            .tool_router
            .call(ToolCallContext::new(self, request, context))
            .await?;
        if let CallToolResponse::Complete(result) = &mut response {
            targets::mark_result(target.as_ref(), &mut result.meta);
        }
        Ok(response)
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let mut tools = self.tool_router.list_all();
        if matches!(self.backend, Backend::Http) {
            let call = http::authenticated_ledger(&context)
                .map_err(|error| http::protocol_error(&error))?
                .clone();
            tools = tokio::task::spawn_blocking(move || {
                let db = call.ledger().lock()?;
                let identity = call.identity();
                db.read_snapshot(|reader| {
                    identity.check_live(reader)?;
                    let enforce = crate::db::policy::get(reader)?.enforce;
                    let mut allowed = Vec::new();
                    for tool in tools {
                        if identity
                            .can_discover(catalog::operation_for_tool(&tool.name)?, enforce)?
                        {
                            allowed.push(tool);
                        }
                    }
                    Ok(allowed)
                })
            })
            .await
            .map_err(|_| rmcp::ErrorData::internal_error("tool discovery failed", None))?
            .map_err(|error| http::protocol_error(&error))?;
        }
        let mut result = rmcp::model::ListToolsResult {
            tools,
            ..Default::default()
        };
        targets::mark_result(self.result_target(&context), &mut result.meta);
        Ok(result)
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("beankeeper", env!("CARGO_PKG_VERSION")))
            .with_instructions(SERVER_INSTRUCTIONS)
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
    let db = Db::open_existing(cli.db_path()?, passphrase.as_ref())?;
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
    let mut server = LedgerServer::with_backend(
        read_only.into(),
        Backend::Stdio {
            ledger: OpenedLedger::new(cli.ledger_target(), db),
            token: cli.agent_token.clone(),
        },
        ActorLabel::configured(cli.actor.as_deref()),
    );
    server.show_client_hint_notice = !cli.verbosity.quiet;
    if let Some(identity) = identity {
        for tool in server.tool_router.list_all() {
            if !identity.can_discover(catalog::operation_for_tool(&tool.name)?, enforce)? {
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
            catalog::operation_for_tool(&tool.name)?
                .permission()
                .primary()?;
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
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
            crate::db::companies::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        for (code, kind) in [
            ("1000", beankeeper::types::AccountType::Asset),
            ("1200", beankeeper::types::AccountType::Asset),
            ("4000", beankeeper::types::AccountType::Revenue),
        ] {
            Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
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
                "security_create" => dispatch::security_create(decode(input)?)?,
                "security_list" => dispatch::security_list(decode(input)?),
                "investment_buy" => dispatch::investment_buy(decode(input)?)?,
                "security_price" => dispatch::security_price(decode(input)?)?,
                "investment_sell" => dispatch::investment_sell(decode(input)?)?,
                "investment_holdings" => dispatch::investment_holdings(decode(input)?),
                "investment_lots" => dispatch::investment_lots(decode(input)?),
                _ => return Err(CliError::General(format!("unexpected test tool: {tool}"))),
            };
            let response = request.execute(&db, &actor)?;
            dispatch::response(&response, &actor).map(|(data, _)| data)
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
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
            crate::db::companies::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
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
            dispatch::report(input)?.execute(&db, &Actor::for_mcp(None, None, None))?;
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
