//! Host tools composed behind the ledger's authenticated HTTP boundary.

use std::future::Future;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
    PaginatedRequestParams, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};

use super::{LedgerServer, http, tool_error};
use crate::capability::Capability;
use crate::error::CliError;
use crate::identity::VerifiedAgent;

/// Whether a host tool can change ledger or host application state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostToolEffect {
    /// Observes state without changing it; available under the read-only ceiling.
    ReadOnly,
    /// Changes state, including UI navigation; removed from read-only routers.
    Mutation,
}

/// A host tool's immutable discovery and authorization contract.
#[derive(Debug, Clone)]
pub struct HostTool {
    /// MCP tool description and input schema.
    pub tool: Tool,
    /// Explicit signed grant required for discovery and execution, even with policy off.
    pub capability: Capability,
    /// Trusted host declaration used to enforce the router's read-only ceiling.
    pub effect: HostToolEffect,
}

/// Application tools served beside the built-in ledger tools on one endpoint.
///
/// The catalog is captured once at construction. Beankeeper filters discovery,
/// checks the verified request identity against the live registry and trust root
/// before dispatch, and enforces each grant even when ledger policy is off.
/// The identity comes from that request, never the session's initialize token.
/// No raw credential or database connection is passed to the handler.
///
/// Hosts must accurately declare effects and validate tool arguments. Async host
/// effects are outside the ledger transaction; identity is an authorization
/// snapshot at dispatch, not a credential to cache for later requests.
pub trait HostToolHandler: Send + Sync + 'static {
    /// Return the startup catalog. Names must be unique across host and ledger tools.
    fn tools(&self) -> Vec<HostTool>;

    /// Execute an authorized host tool with the verified identity of this request.
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        identity: VerifiedAgent,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send;
}

pub(super) struct NoHostTools;

impl HostToolHandler for NoHostTools {
    fn tools(&self) -> Vec<HostTool> {
        Vec::new()
    }

    fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _identity: VerifiedAgent,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send {
        std::future::ready(Err(ErrorData::invalid_params("unknown host tool", None)))
    }
}

pub(super) fn catalog(
    tools: Vec<HostTool>,
    read_only: bool,
) -> Result<Arc<Vec<HostTool>>, CliError> {
    let mut names: std::collections::HashSet<_> = LedgerServer::tool_router()
        .list_all()
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect();
    for entry in &tools {
        if entry.tool.name.is_empty() || !names.insert(entry.tool.name.to_string()) {
            return Err(CliError::Validation(
                "host tool names must be nonempty and unique across host and ledger tools".into(),
            ));
        }
    }
    Ok(Arc::new(
        tools
            .into_iter()
            .filter(|tool| !read_only || tool.effect == HostToolEffect::ReadOnly)
            .collect(),
    ))
}

pub(super) struct HostServer<H> {
    pub ledger: LedgerServer,
    pub host: Arc<H>,
    pub tools: Arc<Vec<HostTool>>,
}

impl<H> HostServer<H> {
    async fn live_identity(call: http::AuthenticatedLedger) -> Result<VerifiedAgent, CliError> {
        tokio::task::spawn_blocking(move || {
            let db = call.ledger().lock()?;
            db.read_snapshot(|reader| {
                crate::db::policy::get(reader)?;
                let identity = call.identity();
                identity.check_live(reader)?;
                Ok(identity.clone())
            })
        })
        .await
        .map_err(|_| CliError::General("host tool authorization failed".into()))?
    }
}

impl<H: HostToolHandler> ServerHandler for HostServer<H> {
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let call = http::authenticated_ledger(&context)
            .map_err(|error| http::protocol_error(&error))?
            .clone();
        let mut result = self.ledger.list_tools(request, context).await?;
        if !call.ledger().is_startup() {
            return Ok(result);
        }
        for entry in self.tools.iter() {
            if call
                .identity()
                .allows(entry.capability)
                .map_err(|error| http::protocol_error(&error))?
            {
                result.tools.push(entry.tool.clone());
            }
        }
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(entry) = self
            .tools
            .iter()
            .find(|entry| entry.tool.name == request.name)
        else {
            return self.ledger.call_tool(request, context).await;
        };
        let call = match http::authenticated_ledger(&context) {
            Ok(call) if call.ledger().is_startup() => call,
            Ok(_) => return self.ledger.call_tool(request, context).await,
            Err(error) => return Ok(tool_error(&error).into()),
        };
        let identity = match Self::live_identity(call.clone()).await {
            Ok(identity) => identity,
            Err(error) => return Ok(tool_error(&error).into()),
        };
        match identity.allows(entry.capability) {
            Ok(true) => self.host.call_tool(request, identity).await.map(Into::into),
            Ok(false) => Ok(tool_error(&CliError::Forbidden(format!(
                "operation requires capability {}",
                entry.capability.path()
            )))
            .into()),
            Err(error) => Ok(tool_error(&error).into()),
        }
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools
            .iter()
            .find(|entry| entry.tool.name == name)
            .map(|entry| entry.tool.clone())
            .or_else(|| self.ledger.get_tool(name))
    }

    fn get_info(&self) -> ServerConfig {
        self.ledger.get_info()
    }
}
