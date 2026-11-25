//! Opt-in MCP provisioning of a separate, host-managed demo ledger.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{
    ErrorData,
    model::{CallToolRequestParams, CallToolResult, Tool, ToolAnnotations},
};
use secrecy::SecretString;
use serde_json::json;

use super::{HostTool, HostToolEffect, HostToolHandler, catalog, tool_error};
use crate::{
    capability::Capability, commands::init::create_demo_ledger_from_profile, db::Actor,
    demo::DemoProfile, error::CliError, identity::VerifiedAgent,
};

/// An opt-in host tool that creates synthetic books in one fresh local file.
///
/// Only the trusted host supplies the destination and optional encryption key.
/// The MCP client submits the same typed profile as the CLI. The host can
/// register the generated ledger as a separate target after creation.
pub struct DemoLedgerTool {
    path: PathBuf,
    passphrase: Option<SecretString>,
}

impl DemoLedgerTool {
    /// Name to use when composing this tool with other application host tools.
    pub const NAME: &str = "demo_ledger_create";

    /// Configure the host-controlled destination and its optional passphrase.
    #[must_use]
    pub fn new(path: PathBuf, passphrase: Option<SecretString>) -> Self {
        Self { path, passphrase }
    }
}

impl HostToolHandler for DemoLedgerTool {
    fn tools(&self) -> Vec<HostTool> {
        vec![HostTool {
            tool: Tool::new(
                Self::NAME,
                "Create a fresh verified demo ledger with fictional books shaped by the interview choices.",
                Arc::new(catalog::input_schema::<DemoProfile>(Self::NAME)),
            )
            .with_annotations(
                ToolAnnotations::new().read_only(false).destructive(false).open_world(false),
            ),
            capability: Capability::Admin,
            effect: HostToolEffect::Mutation,
        }]
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        identity: VerifiedAgent,
    ) -> Result<CallToolResult, ErrorData> {
        if request.name != Self::NAME {
            return Err(ErrorData::invalid_params("unknown demo tool", None));
        }
        let profile = match DemoProfile::try_from(request.arguments.unwrap_or_default()) {
            Ok(profile) => profile,
            Err(error) => return Ok(tool_error(&error)),
        };
        let actor = match identity.uri().parse() {
            // The host grant is verified against the startup ledger. Record
            // that caller's label without transferring authority to a new root.
            Ok(label) => Actor::new(label),
            Err(error) => return Ok(tool_error(&CliError::from(error))),
        };
        let path = self.path.clone();
        let passphrase = self.passphrase.clone();
        let result = tokio::task::spawn_blocking(move || {
            create_demo_ledger_from_profile(&path, passphrase.as_ref(), &actor, &profile)?;
            Ok(CallToolResult::structured(json!({
                "summary": "Created verified synthetic demo books.",
                "is_demo": true,
                "books": profile.books().len(),
            })))
        })
        .await;
        Ok(match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => tool_error(&error),
            Err(_) => tool_error(&CliError::General("demo creation did not complete".into())),
        })
    }
}
