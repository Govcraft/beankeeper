# Authenticated MCP host tools

Rust applications can expose their own tools beside Beankeeper's ledger tools on
one authenticated `/mcp` endpoint. Implement `HostToolHandler` and pass it to
`http_router_with_host_tools(&cli, passphrase.as_ref(), read_only, config, host)`.
The factory accepts an already-resolved secret, like
`http_router_with_passphrase`. It does not bind a listener or start a runtime.
Merge the result into the host's axum router and cancel the transport's
`cancellation_token` during shutdown. Sibling routes keep their own middleware.

## Navigation authority

Issue a UI-scoped token through the existing CLI and encrypted ledger root:

```sh
bk --json agent issue --name desktop --generate-key --caps ui/navigate --ttl 8h
```

The signed grant remains exactly `ui/navigate`; the subject is
`agent://<root>/ui/<agent-id>`. Existing ledger tokens keep their
`agent://<root>/ledger/<agent-id>` subjects and grants. The attestation builder,
issuer and verifier retain their normal scope validation. No schema change or
new trust root is needed. UI tokens use the same registry, audit and revocation
commands as ledger tokens.

Grants within one token must all be strictly beneath `ledger/` or all strictly
beneath `ui/`. Bare `ledger` and `ui`, empty grant lists, unknown namespaces, and
mixed lists such as `ledger/read,ui/navigate` are rejected. The attestation format
requires a nonempty subject scope and cannot span these sibling namespaces.
Issue separate tokens and select the appropriate bearer for each request. One
MCP transport session can carry both kinds of requests. A host can instead start
separate sessions when its client cannot vary bearer credentials per request.

`Capability::Navigate` names `ui/navigate`.
`VerifiedAgent::allows(Capability::Navigate)` checks the signed grants using
segment-aware coverage. `ui/navigate/panel` cannot authorize `ui/navigate`.
`identity::authorize` always requires navigation authority, including with ledger
enforcement off. Ledger read, post and admin grants never confer navigation.
UI grants never confer ledger grants. Ledger commands retain their existing
policy-off behavior, which permits operations without an explicit ledger grant;
enable ledger enforcement to require grants for those commands.

## Host authorization and read-only behavior

Each `HostTool` supplies an MCP definition, one required `Capability`, and a
`HostToolEffect`. The catalog is captured at construction; duplicate names,
including collisions with any built-in ledger tool, fail construction.

Every HTTP request passes the existing bearer authentication middleware.
An explicit Authorization header overrides the CLI fallback, including when the
explicit credential is invalid. The current request's credential is re-verified
against live expiry and revocations before discovery or host dispatch. No
credential is retained as session authority. The callback receives that request's
`VerifiedAgent`, with its URI, token ID and signed grants, without a raw token.
All host tools require their declared signed grants even when ledger enforcement
is off. Built-in tools retain live policy checks and atomic ledger execution.

`read_only=true` removes every tool declared `HostToolEffect::Mutation` from both
discovery and dispatch. Navigation changes host state and must be declared a
mutation. Observation-only tools may declare `ReadOnly`. A grant cannot override
this ceiling. The host is trusted to declare effects accurately, validate its
arguments, and implement only the named authorized action. The host callback is
async and runs outside the ledger transaction; the identity is a snapshot at
dispatch, so do not cache it as authority for later requests.

## Dispatch to another ledger

Use `http_router_with_ledger_targets` to serve built-in tools against multiple
opened ledgers on the same MCP endpoint. Construct `LedgerTargets::new(startup)`
with the primary `Db` and pass a clone to the router factory. Keep the original
to register additional ledgers as they become available, even after clients
initialize a session.

```rust
use beankeeper_cli::{db::Db, mcp::{LedgerTargets, http_router_with_ledger_targets}};

let startup = Db::open_existing(books_path, Some(&books_key))?;
let targets = LedgerTargets::new(startup);
let router = http_router_with_ledger_targets(&cli, targets.clone(), false, config, host)?;

// The host can do this later, using a separately initialized demo ledger.
let demo = Db::open_existing(demo_path, Some(&demo_key))?;
targets.register("demo".parse()?, demo)?;
```

Send `Beankeeper-Ledger-Target: demo` with the demo ledger's bearer on each
`tools/list` or `tools/call` request that targets it. Reuse the existing
`Mcp-Session-Id`. Trusted axum middleware can instead insert a typed
`LedgerTarget` into the request extensions, which takes precedence over the
header. With neither selector, the request uses `LedgerTarget::default()`
(`startup`).

Each ledger has its own trust root and registry. For a newly initialized
encrypted demo ledger, initialize its root and issue its grants before serving
it. Beankeeper verifies the selected ledger's bearer and rechecks its live
revocations, grant requirements, and policy in the same snapshot or write
transaction that executes the tool. Direct grants use the ordinary posting path,
and signed propose-only grants queue a proposal. Audit entries carry the
verified caller's attribution. The read-only ceiling applies to every target.

Use startup credentials and select the startup ledger for application host tools.
The configured token fallback also applies there. Multi-ledger endpoints disable
SSE replay with `Last-Event-ID` because the transport cache is session-scoped.
For a lost response to a write, use the tool's existing idempotency mechanism
or check the selected ledger before retrying.

## Example host

This example queues a typed navigation request for an application's UI loop.
The host application owns the receiver, view handling, listener and shutdown.
It requires the host's usual axum, rmcp, serde, schemars, serde_json and secrecy
dependencies, using versions compatible with Beankeeper.

```no_run
use std::{future::Future, sync::mpsc::Sender};
use beankeeper_cli::{
    capability::Capability,
    cli::Cli,
    error::CliError,
    identity::VerifiedAgent,
    mcp::{HostTool, HostToolEffect, HostToolHandler, StreamableHttpServerConfig,
          http_router_with_host_tools},
};
use rmcp::{ErrorData, model::{CallToolRequestParams, CallToolResult, Tool}};
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum View { Home, Accounts }

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Navigate { view: View }

struct NavigationHost { sender: Sender<View> }

impl HostToolHandler for NavigationHost {
    fn tools(&self) -> Vec<HostTool> {
        vec![HostTool {
            tool: Tool::new("navigate", "Navigate the host UI",
                            schemars::schema_for!(Navigate).to_value()
                                .as_object().cloned().unwrap_or_default()),
            capability: Capability::Navigate,
            effect: HostToolEffect::Mutation,
        }]
    }

    fn call_tool(&self, request: CallToolRequestParams, identity: VerifiedAgent)
        -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send
    {
        let result = match serde_json::from_value::<Navigate>(
            Value::Object(request.arguments.unwrap_or_default()))
        {
            Ok(input) => match self.sender.send(input.view) {
                Ok(()) => CallToolResult::structured(json!({
                    "queued": true, "requested_by": identity.uri()
                })),
                Err(_) => CallToolResult::structured_error(json!({
                    "code": "UI_UNAVAILABLE", "message": "UI loop is closed"
                })),
            },
            Err(error) => CallToolResult::structured_error(json!({
                "code": "VALIDATION", "message": error.to_string()
            })),
        };
        std::future::ready(Ok(result))
    }
}

fn host_router(cli: &Cli, passphrase: Option<&SecretString>, sender: Sender<View>)
    -> Result<axum::Router, CliError>
{
    let config = StreamableHttpServerConfig::default().enforce_origin_validation();
    // Retain config.cancellation_token.clone() in the application's shutdown state.
    http_router_with_host_tools(cli, passphrase, false, config, NavigationHost { sender })
}
# fn main() {}
```

The built-in CLI and stdio server do not register host tools. View URLs, browser
sessions, event delivery, and Desktop's specific navigation tools belong to the
host application.
