//! MCP discovery and dispatch generated from the operation catalog.

use rmcp::{
    RoleServer,
    handler::server::router::tool::{ToolRoute, ToolRouter},
    model::{CallToolResponse, CallToolResult, Tool, ToolAnnotations},
    service::RequestContext,
};
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
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

pub(super) fn input_schema<Input: JsonSchema>(name: &str) -> Map<String, Value> {
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
    schema
}

fn tool<Input: JsonSchema>(
    operation: Operation,
    name: &'static str,
    description: &'static str,
) -> Tool {
    Tool::new(name, description, Arc::new(input_schema::<Input>(name)))
        .with_annotations(ToolAnnotations::new().read_only(operation.is_read_only()))
}

/// An immutable operation paired with its generated MCP definition.
///
/// Entries can only be constructed by the canonical operation catalog.
#[derive(Debug, Clone)]
pub struct LedgerTool {
    operation: Operation,
    tool: Tool,
}

impl LedgerTool {
    /// The typed authorization and dispatch identity.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        self.operation
    }

    /// The MCP discovery definition.
    #[must_use]
    pub const fn tool(&self) -> &Tool {
        &self.tool
    }

    /// Consume the entry as its operation and MCP definition.
    #[must_use]
    pub fn into_parts(self) -> (Operation, Tool) {
        (self.operation, self.tool)
    }
}

macro_rules! catalog_entry {
    ($entries:ident, $access:ident, $variant:ident, tool($name:ident, $input:ident, $description:literal)) => {
        if matches!($access, LedgerAccess::ReadWrite) || Operation::$variant.is_read_only() {
            $entries.push(LedgerTool {
                operation: Operation::$variant,
                tool: tool::<inputs::$input>(Operation::$variant, stringify!($name), $description),
            });
        }
    };
    ($entries:ident, $access:ident, $variant:ident, tool_at($path:literal, $name:ident, $input:ident, $description:literal)) => {
        catalog_entry!(
            $entries,
            $access,
            $variant,
            tool($name, $input, $description)
        );
    };
    ($entries:ident, $access:ident, $variant:ident, mcp_only($reason:ident, $name:ident, $input:ident, $description:literal)) => {
        catalog_entry!(
            $entries,
            $access,
            $variant,
            tool($name, $input, $description)
        );
    };
    ($entries:ident, $access:ident, $variant:ident, grouped($group:ident, $path:literal)) => {};
    ($entries:ident, $access:ident, $variant:ident, cli_only($reason:ident)) => {};
}

macro_rules! typed_catalog {
    ($($variant:ident => ($name:literal, $permission:ident, $exposure:ident($($details:tt)*))),+ $(,)?) => {
        pub(super) fn entries(access: LedgerAccess) -> Vec<LedgerTool> {
            let mut entries = Vec::new();
            $(catalog_entry!(entries, access, $variant, $exposure($($details)*));)+
            entries
        }
    };
}

ledger_operations!(typed_catalog);

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
                Err(error) if operation == Operation::TxnMetaSet => super::tool_error(
                    &CliError::Validation(format!("{error}. {}", inputs::ANNOTATION_VALUES_HINT)),
                ),
                Err(error) if operation == Operation::TxnMetaUnset => super::tool_error(
                    &CliError::Validation(format!("{error}. {}", inputs::ANNOTATION_KEYS_HINT)),
                ),
                Err(error)
                    if matches!(
                        operation,
                        Operation::AccountRename
                            | Operation::AccountCreate
                            | Operation::LedgerSetupAccountAdd
                    ) =>
                {
                    super::tool_error(&CliError::Validation(error.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_catalog_covers_every_registered_tool_once() -> Result<(), CliError> {
        for access in [LedgerAccess::ReadOnly, LedgerAccess::ReadWrite] {
            let entries = access.catalog();
            let router = router(access);
            let tools = router.list_all();
            assert_eq!(entries.len(), tools.len());
            let mut names = std::collections::HashSet::new();
            for entry in &entries {
                let (operation, tool) = entry.clone().into_parts();
                assert!(names.insert(tool.name.clone()));
                assert_eq!(operation_for_tool(&tool.name)?, operation);
                assert_eq!(
                    tools.iter().find(|registered| registered.name == tool.name),
                    Some(&tool)
                );
            }
            for operation in Operation::ALL {
                let expected = matches!(
                    operation.exposure(),
                    Exposure::Both(_) | Exposure::McpOnly(_, _)
                ) && (matches!(access, LedgerAccess::ReadWrite)
                    || operation.is_read_only());
                assert_eq!(
                    entries.iter().any(|entry| entry.operation() == *operation),
                    expected
                );
            }
        }
        Ok(())
    }
}
