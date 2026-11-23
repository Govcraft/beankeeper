//! MCP discovery and dispatch generated from the operation catalog.

use rmcp::{
    RoleServer,
    handler::server::{
        common::{FromContextPart, schema_for_input},
        router::tool::{ToolRoute, ToolRouter},
        wrapper::Parameters,
    },
    model::{CallToolResponse, CallToolResult, Tool, ToolAnnotations},
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

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

fn tool<Input: JsonSchema + 'static>(
    operation: Operation,
    name: &'static str,
    description: &'static str,
) -> Tool {
    let schema = schema_for_input::<Input>()
        .unwrap_or_else(|error| panic!("invalid schema for {name}: {error}"));
    Tool::new(name, description, schema)
        .with_annotations(ToolAnnotations::new().read_only(operation.is_read_only()))
}

fn route<Input, Handle, Future>(tool: Tool, handle: Handle) -> ToolRoute<LedgerServer>
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
        let input = Parameters::<Input>::from_context_part(&mut context);
        let handle = handle.clone();
        Box::pin(async move {
            let Parameters(input) = input?;
            let result = handle(context.service.clone(), input, context.request_context).await;
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
