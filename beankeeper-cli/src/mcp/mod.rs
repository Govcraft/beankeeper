//! Transport-independent MCP tools and the local stdio server.
//!
//! Tool bodies use the same in-process database operations and JSON renderers
//! as the CLI. Only the transport writes to stdout.

mod inputs;

use std::sync::{Arc, Mutex};

use clap::Parser;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use secrecy::ExposeSecret;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::capability::Capability;
use crate::cli::{Cli, Command};
use crate::commands;
use crate::db::{Actor, connection::Db};
use crate::error::CliError;

/// A ledger tool router, independent of the transport used to serve it.
#[derive(Clone)]
pub struct LedgerServer {
    db: Arc<Mutex<Db>>,
    tool_router: ToolRouter<Self>,
    agent_token: Option<secrecy::SecretString>,
}

impl LedgerServer {
    /// Construct a router over an already opened database.
    ///
    /// Read-only routers do not register mutation tools at all.
    #[must_use]
    pub fn new(db: Db, read_only: bool) -> Self {
        let mut tool_router = Self::tool_router();
        if read_only {
            for tool in tool_router.list_all() {
                if Capability::for_operation(&tool.name).ok() != Some(Capability::Read) {
                    tool_router.remove_route(&tool.name);
                }
            }
        }
        Self {
            db: Arc::new(Mutex::new(db)),
            tool_router,
            agent_token: None,
        }
    }

    async fn execute<T: Serialize>(
        &self,
        tool: &'static str,
        input: T,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let actor = client_actor(
            context
                .peer
                .peer_info()
                .map(|info| info.client_info.name.clone())
                .as_deref(),
        );
        let input = match serde_json::to_value(input) {
            Ok(input) => input,
            Err(error) => return tool_error(&CliError::General(error.to_string())),
        };
        let db = self.db.clone();
        let token = self.agent_token.clone();
        let result = tokio::task::spawn_blocking(move || {
            let db = db
                .lock()
                .map_err(|_| CliError::Database("ledger connection is unavailable".into()))?;
            // One snapshot covers authorization and execution, including policy
            // changes made by another process while this session is running.
            crate::db::audit::atomically(db.conn(), || {
                let identity = crate::identity::authorize(
                    db.conn(),
                    token.as_ref().map(ExposeSecret::expose_secret),
                    Capability::for_operation(tool)?,
                )?;
                let actor = match identity {
                    Some(identity) => actor.with_identity(identity),
                    None => actor,
                };
                execute_operation(&db, tool, input, &actor)
            })
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
}

fn tool_error(error: &CliError) -> CallToolResult {
    CallToolResult::structured_error(
        json!({"code":error.error_code(), "message":error.to_string()}),
    )
}

fn client_actor(name: Option<&str>) -> Actor {
    let name: String = name
        .unwrap_or_default()
        .chars()
        .take(80)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-');
    Actor::new(format!(
        "mcp:{}",
        if name.is_empty() { "unknown" } else { name }
    ))
}

// Parse flags in memory to reuse CLI defaults, enums and argument constraints.
// There is no subprocess or output-producing CLI dispatch on this path.
fn operation_cli(tool: &str, input: &mut Map<String, Value>) -> Result<Cli, CliError> {
    let mut args = vec!["bk".to_string(), "--json".into(), "--quiet".into()];
    if tool != "audit_log"
        && let Some(Value::String(company)) = input.remove("company")
    {
        args.push(format!("--company={company}"));
    }
    let positional: &[&str] = match tool {
        "security_create" | "security_price" | "investment_buy" | "investment_sell"
        | "investment_lots" => &["symbol"],
        "company_show" => &["slug"],
        "account_show" => &["code"],
        "account_create" => &["code", "name"],
        "txn_show" | "txn_reverse" => &["id"],
        "txn_meta_set" => &["id", "values"],
        "txn_meta_unset" => &["id", "keys"],
        "txn_clear" => &["transaction_id"],
        "budget_set" | "budget_delete" => &["account"],
        _ => &[],
    };
    if tool == "report" {
        args.push("report".into());
        let kind = input
            .remove("kind")
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| CliError::Usage("report kind is required".into()))?;
        args.push(kind);
    } else if let Some(action) = tool.strip_prefix("txn_meta_") {
        args.extend(["txn".into(), "meta".into(), action.into()]);
    } else if let Some((group, action)) = tool.split_once('_') {
        args.extend([group.into(), action.into()]);
    } else {
        args.push(tool.into());
    }
    if tool == "budget_delete" {
        args.push("--force".into());
    }
    for (key, value) in input
        .iter()
        .filter(|(key, _)| !positional.contains(&key.as_str()))
    {
        let flag = if key == "account_type" {
            "type".into()
        } else {
            key.replace('_', "-")
        };
        append_flag(&mut args, &flag, value)?;
    }
    let positions: Vec<String> = positional
        .iter()
        .filter_map(|key| input.remove(*key))
        .filter(|v| !v.is_null())
        .flat_map(|value| match value {
            Value::Array(values) => values,
            value => vec![value],
        })
        .map(|value| scalar(&value))
        .collect::<Result<_, _>>()?;
    if !positions.is_empty() {
        args.push("--".into());
        args.extend(positions);
    }
    Cli::try_parse_from(args).map_err(|error| CliError::Usage(error.to_string()))
}

fn scalar(value: &Value) -> Result<String, CliError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(CliError::Usage(
            "expected a string or numeric argument".into(),
        )),
    }
}

fn append_flag(args: &mut Vec<String>, name: &str, value: &Value) -> Result<(), CliError> {
    match value {
        Value::Null | Value::Bool(false) => (),
        Value::Bool(true) => args.push(format!("--{name}")),
        Value::Array(values) => {
            for value in values {
                append_flag(args, name, value)?;
            }
        }
        _ => args.push(format!("--{name}={}", scalar(value)?)),
    }
    Ok(())
}

fn prepare_post(input: &mut Map<String, Value>) -> Result<(), CliError> {
    for (source, target) in [("debits", "debit"), ("credits", "credit")] {
        let lines: Vec<inputs::Line> =
            serde_json::from_value(input.remove(source).unwrap_or(Value::Null))
                .map_err(|error| CliError::Usage(error.to_string()))?;
        let values = lines
            .into_iter()
            .map(|line| {
                if line.account.contains(':') || line.amount.contains(':') {
                    return Err(CliError::Validation(
                        "entry account and amount cannot contain ':'".into(),
                    ));
                }
                Ok(Value::String(format!(
                    "{}:{}:{}",
                    line.account,
                    line.amount,
                    line.memo.unwrap_or_default()
                )))
            })
            .collect::<Result<Vec<_>, CliError>>()?;
        input.insert(target.into(), Value::Array(values));
    }
    if let Some(Value::Object(meta)) = input.remove("meta") {
        input.insert(
            "meta".into(),
            Value::Array(
                meta.into_iter()
                    .map(|(key, value)| {
                        scalar(&value).map(|value| Value::String(format!("{key}={value}")))
                    })
                    .collect::<Result<_, _>>()?,
            ),
        );
    }
    Ok(())
}

fn execute_operation(
    db: &Db,
    tool: &str,
    input: Value,
    actor: &Actor,
) -> Result<(Value, Option<Value>), CliError> {
    let Value::Object(mut input) = input else {
        return Err(CliError::Usage("tool arguments must be an object".into()));
    };
    if tool == "txn_post" {
        prepare_post(&mut input)?;
    }
    if tool.starts_with("txn_meta_") {
        let has_id = input.get("id").is_some_and(|value| !value.is_null());
        let has_reference = input.get("reference").is_some_and(|value| !value.is_null());
        if has_id == has_reference {
            return Err(CliError::Usage(
                "provide exactly one transaction ID or reference".into(),
            ));
        }
    }
    let batch = input
        .remove("batch")
        .filter(|value| !value.is_null())
        .map(|value| {
            let lines: Vec<inputs::BatchLine> = serde_json::from_value(value)
                .map_err(|error| CliError::Usage(error.to_string()))?;
            Ok::<_, CliError>(
                lines
                    .into_iter()
                    .enumerate()
                    .map(|(index, line)| commands::txn_clear::BatchLine {
                        line: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                        reference: line.reference,
                        amount: line.amount,
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .transpose()?;
    if batch.is_some() {
        input.insert("from_file".into(), Value::String("-".into()));
    }
    let cli = operation_cli(tool, &mut input)?;
    let company = cli.company.as_deref().unwrap_or_default();
    match &cli.command {
        Command::Security(args) => {
            commands::investment::execute_security(db, company, &args.command, actor)
        }
        Command::Investment(args) => {
            commands::investment::execute_investment(db, company, &args.command, actor)
        }
        Command::Company(sub) => commands::company::execute_json(db, sub),
        Command::Account(args) => {
            commands::account::execute_json_with_actor(db, company, &args.command, actor)
        }
        Command::Txn(args) if tool == "txn_clear" => {
            commands::txn_clear::execute_json(db, company, &args.command, actor, batch.as_deref())
        }
        Command::Txn(args) => {
            return commands::txn::execute_json_with_pagination(
                &cli,
                db,
                company,
                &args.command,
                actor,
            );
        }
        Command::Report(args) => commands::report::execute_json(&cli, db, company, &args.command),
        Command::Budget(args) => commands::budget::execute_json(db, company, &args.command, actor),
        Command::Audit(args) => {
            let data = commands::audit::execute_json(db, &args.command)?;
            let pagination = commands::audit::pagination(
                db,
                &args.command,
                data.as_array().map_or(0, Vec::len),
            )?;
            let pagination = serde_json::to_value(pagination)
                .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))?;
            return Ok((data, Some(pagination)));
        }
        Command::Verify => commands::verify::execute_json(db),
        _ => Err(CliError::Usage(
            "operation is not available over MCP".into(),
        )),
    }
    .map(|data| (data, None))
}

#[tool_router]
impl LedgerServer {
    #[tool(description = "Create a company-scoped security with a fixed quotation currency.")]
    async fn security_create(
        &self,
        Parameters(input): Parameters<inputs::SecurityCreate>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("security_create", input, context).await
    }

    #[tool(description = "List company securities.")]
    async fn security_list(
        &self,
        Parameters(input): Parameters<inputs::CompanyScope>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("security_list", input, context).await
    }

    #[tool(description = "Record a dated price per whole share in major currency units.")]
    async fn security_price(
        &self,
        Parameters(input): Parameters<inputs::SecurityPrice>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("security_price", input, context).await
    }

    #[tool(
        description = "Acquire an exact share quantity and post total acquisition cost against cash. A duplicate reference fails atomically."
    )]
    async fn investment_buy(
        &self,
        Parameters(input): Parameters<inputs::InvestmentBuy>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("investment_buy", input, context).await
    }

    #[tool(
        description = "Sell shares with FIFO by default, LIFO, or one specific lot; post proceeds, basis relief and realized gain or loss atomically."
    )]
    async fn investment_sell(
        &self,
        Parameters(input): Parameters<inputs::InvestmentSell>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("investment_sell", input, context).await
    }

    #[tool(description = "List acquisition lots with remaining share quantities and cost basis.")]
    async fn investment_lots(
        &self,
        Parameters(input): Parameters<inputs::InvestmentLots>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("investment_lots", input, context).await
    }

    #[tool(
        description = "Value holdings at the latest price on or before as_of without posting unrealized gains. Monetary results use minor units."
    )]
    async fn investment_holdings(
        &self,
        Parameters(input): Parameters<inputs::InvestmentHoldings>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("investment_holdings", input, context).await
    }

    #[tool(description = "List companies in the ledger.")]
    async fn company_list(
        &self,
        Parameters(input): Parameters<inputs::Empty>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("company_list", input, context).await
    }

    #[tool(description = "Show one company by slug.")]
    async fn company_show(
        &self,
        Parameters(input): Parameters<inputs::CompanyShow>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("company_show", input, context).await
    }

    #[tool(
        description = "List accounts, optionally filtering by type or name and including balances."
    )]
    async fn account_list(
        &self,
        Parameters(input): Parameters<inputs::AccountList>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("account_list", input, context).await
    }

    #[tool(description = "Show one account in a company.")]
    async fn account_show(
        &self,
        Parameters(input): Parameters<inputs::AccountShow>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("account_show", input, context).await
    }

    #[tool(description = "Create an account in a company.")]
    async fn account_create(
        &self,
        Parameters(input): Parameters<inputs::AccountCreate>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("account_create", input, context).await
    }

    #[tool(
        description = "Post balanced debits and credits in decimal major units. Always pass reference and on_conflict=skip for safe retries. Conflict strategies are error or skip. Use txn_meta_set/unset to annotate existing postings."
    )]
    async fn txn_post(
        &self,
        Parameters(input): Parameters<inputs::TxnPost>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("txn_post", input, context).await
    }

    #[tool(
        description = "Reverse a transaction with a linked mirror posting. Pass the same explicit reference on retries to return the existing reversal. Reconciled transactions cannot be reversed; existing clearings stay on the original."
    )]
    async fn txn_reverse(
        &self,
        Parameters(input): Parameters<inputs::TxnReverse>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("txn_reverse", input, context).await
    }

    #[tool(
        description = "Add or replace transaction annotations as key=value strings. Provide company and exactly one of id or reference. Relationship keys are reserved. No-op retries write no audit row."
    )]
    async fn txn_meta_set(
        &self,
        Parameters(input): Parameters<inputs::TxnMetaSet>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("txn_meta_set", input, context).await
    }

    #[tool(
        description = "Remove transaction annotation keys. Provide company and exactly one of id or reference. Relationship keys are reserved; absent keys are a no-op."
    )]
    async fn txn_meta_unset(
        &self,
        Parameters(input): Parameters<inputs::TxnMetaUnset>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("txn_meta_unset", input, context).await
    }

    #[tool(description = "List or count transactions using the CLI filters and pagination.")]
    async fn txn_list(
        &self,
        Parameters(input): Parameters<inputs::TxnList>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("txn_list", input, context).await
    }

    #[tool(description = "Show transaction entries, attachments, and clearing details.")]
    async fn txn_show(
        &self,
        Parameters(input): Parameters<inputs::TxnShow>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("txn_show", input, context).await
    }

    #[tool(
        description = "Run a financial report. Select kind and that report's options; results match the CLI JSON data payload."
    )]
    async fn report(
        &self,
        Parameters(input): Parameters<inputs::Report>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("report", input, context).await
    }

    #[tool(description = "List budget entries for a company and year.")]
    async fn budget_list(
        &self,
        Parameters(input): Parameters<inputs::BudgetList>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("budget_list", input, context).await
    }

    #[tool(
        description = "Set a monthly amount or distribute an annual budget across twelve months."
    )]
    async fn budget_set(
        &self,
        Parameters(input): Parameters<inputs::BudgetSet>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("budget_set", input, context).await
    }

    #[tool(description = "Delete a monthly or annual budget; this call confirms deletion.")]
    async fn budget_delete(
        &self,
        Parameters(input): Parameters<inputs::BudgetDelete>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("budget_delete", input, context).await
    }

    #[tool(
        description = "Clear or reconcile entries by transaction and entry IDs, reference and account, or an atomic in-memory batch."
    )]
    async fn txn_clear(
        &self,
        Parameters(input): Parameters<inputs::TxnClear>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("txn_clear", input, context).await
    }

    #[tool(
        description = "Find orphaned intercompany correlations across the ledger without changing it."
    )]
    async fn txn_reconcile(
        &self,
        Parameters(input): Parameters<inputs::Empty>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("txn_reconcile", input, context).await
    }

    #[tool(description = "Verify ledger integrity.")]
    async fn verify(
        &self,
        Parameters(input): Parameters<inputs::Empty>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("verify", input, context).await
    }

    #[tool(description = "List audit changes, optionally filtered by company, entity, or action.")]
    async fn audit_log(
        &self,
        Parameters(input): Parameters<inputs::AuditLog>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.execute("audit_log", input, context).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LedgerServer {
    // Override the SDK macro's async list method: listing is synchronous.
    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::ListToolsResult, rmcp::ErrorData>>
    + Send
    + '_ {
        std::future::ready(Ok(rmcp::model::ListToolsResult {
            tools: self.tool_router.list_all(),
            ..Default::default()
        }))
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("beankeeper", env!("CARGO_PKG_VERSION")))
            .with_instructions("Amounts in inputs use decimal major-unit strings; amounts in results retain integer minor units and add decimal-string _display companions. txn_list and audit_log return pagination in result _meta.pagination. Always pass a reference to txn_post, and on_conflict=skip for safe retries. Company is required for company-scoped tools. Never pass database paths or secrets to tools.")
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
        Some(crate::identity::verify(db.conn(), token.expose_secret())?)
    } else {
        None
    };
    let enforce = crate::db::policy::get(db.conn())?.enforce;
    if enforce && identity.is_none() {
        return Err(CliError::Unauthorized(
            "ledger enforcement requires an agent token".into(),
        ));
    }
    let mut server = LedgerServer::new(db, read_only);
    if enforce && let Some(identity) = identity {
        for tool in server.tool_router.list_all() {
            if !identity.allows(Capability::for_operation(&tool.name)?)? {
                server.tool_router.remove_route(&tool.name);
            }
        }
    }
    server.agent_token.clone_from(&cli.agent_token);
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
    use super::*;

    #[test]
    fn every_mcp_tool_has_an_explicit_requirement() -> Result<(), CliError> {
        let server = LedgerServer::new(Db::open_in_memory()?, false);
        for tool in server.tool_router.list_all() {
            Capability::for_operation(&tool.name)?;
        }
        Ok(())
    }

    #[test]
    fn investment_read_only_router_omits_mutations() -> Result<(), CliError> {
        let server = LedgerServer::new(Db::open_in_memory()?, true);
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

    #[test]
    fn investment_tools_share_trade_valuation_and_actor_behavior() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        crate::db::companies::create_company(db.conn(), "acme", "Acme", None)?;
        for (code, kind) in [("1000", "asset"), ("1200", "asset"), ("4000", "revenue")] {
            crate::db::accounts::create_account(db.conn(), "acme", code, code, kind, None)?;
        }
        let actor = client_actor(Some("Investment Client"));
        let call = |tool, mut input: Value| {
            input["company"] = json!("acme");
            execute_operation(&db, tool, input, &actor).map(|(data, _)| data)
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
        let posted_by: String = db.conn().query_row(
            "SELECT posted_by FROM transactions WHERE reference = 'sell-test'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(posted_by, "mcp:investment-client");
        crate::db::audit::verify_hash_chain(db.conn())?;
        crate::db::audit::verify_creations(db.conn())?;
        Ok(())
    }

    #[test]
    fn actor_names_are_bounded_and_sanitized() {
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
        crate::db::companies::create_company(db.conn(), "acme", "Acme", None)?;
        crate::db::accounts::create_account(db.conn(), "acme", "1000", "Cash", "asset", None)?;
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
            let input = serde_json::to_value(input)
                .map_err(|error| CliError::General(error.to_string()))?;
            execute_operation(&db, "report", input, &client_actor(None))?;
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
