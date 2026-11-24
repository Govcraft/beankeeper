//! MCP discovery and dispatch generated from the operation catalog.

use rmcp::{
    RoleServer,
    handler::server::router::tool::{ToolRoute, ToolRouter},
    model::{CallToolResponse, CallToolResult, Tool, ToolAnnotations},
    service::RequestContext,
};
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

use super::{
    LedgerAccess, LedgerServer,
    dispatch::{self, IntoRequest},
    inputs,
};
use crate::{
    error::CliError,
    operation::{Exposure, Operation, catalog::ledger_operations},
};

pub(crate) fn operation_for_tool(name: &str) -> Result<Operation, CliError> {
    Operation::ALL
        .iter()
        .copied()
        .find(|operation| matches!(operation.exposure(), Exposure::Both(tool) | Exposure::McpOnly(tool, _) if tool == name))
        .ok_or_else(|| CliError::Usage("operation has no capability mapping".into()))
}

fn tool<Input: JsonSchema>(
    operation: Operation,
    name: &'static str,
    description: &'static str,
) -> Tool {
    // Some MCP clients do not resolve references when assembling tool arguments.
    let mut settings = SchemaSettings::draft2020_12();
    settings.inline_subschemas = true;
    let Value::Object(mut schema) = settings
        .into_generator()
        .into_root_schema_for::<Input>()
        .to_value()
    else {
        panic!("input schema for {name} must be a JSON object");
    };
    assert_eq!(
        schema.get("type").and_then(Value::as_str),
        Some("object"),
        "input schema for {name} must declare type: object"
    );
    schema.remove("title");
    schema.remove("description");
    Tool::new(name, description, Arc::new(schema))
        .with_annotations(ToolAnnotations::new().read_only(operation.is_read_only()))
}

fn route<Input, Handle, Future>(
    operation: Operation,
    tool: Tool,
    handle: Handle,
) -> ToolRoute<LedgerServer>
where
    Input: DeserializeOwned + Send + 'static,
    Handle: Fn(LedgerServer, Input, RequestContext<RoleServer>) -> Future
        + Clone
        + Send
        + Sync
        + 'static,
    Future: std::future::Future<Output = CallToolResult> + Send + 'static,
{
    ToolRoute::<LedgerServer>::new_dyn(tool, move |mut context| {
        let arguments = Value::Object(context.arguments.take().unwrap_or_default());
        let input = Input::deserialize(&arguments);
        let handle = handle.clone();
        Box::pin(async move {
            let result = match input {
                Ok(input) => handle(context.service.clone(), input, context.request_context).await,
                Err(error) if operation == Operation::Summary => {
                    let company = arguments.get("company").and_then(Value::as_str);
                    let validation = CliError::Validation(error.to_string());
                    super::summary_error(
                        company,
                        super::tool_error(&validation).structured_content.as_ref(),
                    )
                }
                Err(error) => super::tool_error(&CliError::Validation(format!(
                    "failed to deserialize parameters: {error}"
                ))),
            };
            Ok(CallToolResponse::from(result))
        })
    })
}

macro_rules! route {
    ($router:ident, $access:ident, $variant:ident, tool_at($path:literal, $name:ident, $input:ident, $description:literal)) => {
        route!(
            $router,
            $access,
            $variant,
            tool($name, $input, $description)
        );
    };
    ($router:ident, $access:ident, $variant:ident, mcp_only($reason:ident, $name:ident, $input:ident, $description:literal)) => {
        route!(
            $router,
            $access,
            $variant,
            tool($name, $input, $description)
        );
    };
    ($router:ident, $access:ident, $variant:ident, grouped($group:ident, $path:literal)) => {};
    ($router:ident, $access:ident, $variant:ident, cli_only($reason:ident)) => {};
    ($router:ident, $access:ident, Summary, tool($name:ident, $input:ident, $description:literal)) => {
        if matches!($access, LedgerAccess::ReadWrite) || Operation::Summary.is_read_only() {
            $router.add_route(route(
                Operation::Summary,
                tool::<inputs::$input>(Operation::Summary, stringify!($name), $description),
                |server, input: inputs::$input, context| async move {
                    server.execute_summary(input, context).await
                },
            ));
        }
    };
    ($router:ident, $access:ident, $variant:ident, tool($name:ident, $input:ident, $description:literal)) => {
        if matches!($access, LedgerAccess::ReadWrite) || Operation::$variant.is_read_only() {
            $router.add_route(route(
                Operation::$variant,
                tool::<inputs::$input>(Operation::$variant, stringify!($name), $description),
                |server, input: inputs::$input, context| async move {
                    server
                        .execute(
                            Operation::$variant,
                            dispatch::$name(input).into_request(),
                            context,
                        )
                        .await
                },
            ));
        }
    };
}

macro_rules! router {
    ($($variant:ident => ($name:literal, $permission:ident, $exposure:ident($($details:tt)*))),+ $(,)?) => {
        pub(super) fn router(access: LedgerAccess) -> ToolRouter<LedgerServer> {
            let mut router = ToolRouter::new();
            $(route!(router, access, $variant, $exposure($($details)*));)+
            router
        }
    };
}

ledger_operations!(router);
