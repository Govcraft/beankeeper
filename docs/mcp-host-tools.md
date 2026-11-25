# Authenticated MCP host tools

Rust applications can expose their own tools beside Beankeeper's ledger tools on
one authenticated `/mcp` endpoint. Implement `HostToolHandler` and pass it to
`http_router_with_host_tools(&cli, passphrase.as_ref(), read_only, config, host)`.
The factory accepts an already-resolved secret, like
`http_router_with_passphrase`. It does not bind a listener or start a runtime.
Merge the result into the host's axum router and cancel the transport's
`cancellation_token` during shutdown. Sibling routes keep their own middleware.

## Open or upgrade before dispatch

Path-based router factories require the running ledger schema. For an older
supported schema, match `CliError::SchemaMismatch { stored, expected }` and use
`VerifiedSnapshot::create` followed by `Db::open_after_snapshot`. Pass the opened
connection to `mcp::serve_with_db` for stdio. When your application owns the
HTTP listener, pass the `Db` directly to `http_router_with_ledger_targets` for
a single ledger with SSE replay, or pass `LedgerTargets` for a switchable
endpoint. Use `serve_http_with_targets` when core should own the listener for
switchable targets.

For a parsed CLI ledger command, use `commands::dispatch_with_db(&mut cli, db, meta)`.
The supplied connection selects the ledger and the parsed CLI still supplies the
actor, token, and output options.

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

## Serve an already-opened startup ledger

Pass the opened `Db` directly to the router when the endpoint serves one ledger.
Clients can resume an SSE stream with `Last-Event-ID` using the same transport
configuration as `http_router_with_host_tools`.

```rust
let router = http_router_with_ledger_targets(&cli, startup_db, false, config, host)?;
```

If the host also prepares files configured through `--ledger`, read the
validated bindings from `Cli`. The startup path is available in `cli.db`.
`ledger_bindings()` returns an error for duplicate names or the reserved
`startup` binding.

```rust
for (name, path) in cli.ledger_bindings()? {
    prepare_named_ledger(name, path.as_ref())?;
}
```

## Dispatch to another ledger

Use `http_router_with_ledger_targets` to serve built-in tools against multiple
opened ledgers on the same MCP endpoint. Construct `LedgerTargets::new(startup)`
with the primary `Db` and pass a clone to the router factory. Keep the original
to register and detach ledgers while clients are connected.

```rust
use beankeeper_cli::{db::Db, mcp::{LedgerTargets, NamedLedgerTarget, http_router_with_ledger_targets}};

let startup = Db::open_existing(books_path, Some(&books_key))?;
let targets = LedgerTargets::new(startup);
let router = http_router_with_ledger_targets(&cli, targets.clone(), false, config, host)?;

// Open a separate ledger using its own passphrase.
let demo = Db::open_existing(demo_path, Some(&demo_key))?;
let demo_target: NamedLedgerTarget = "demo".parse()?;
targets.register_host_owned(demo_target.clone().into(), demo)?;

// Later, from an async host task:
let detached = targets.detach(&demo_target).await?;
let released_path = detached.path();
```

Send `Beankeeper-Ledger-Target: demo` with the startup ledger's bearer on each
`tools/list` or `tools/call` request that targets it. Reuse the existing
`Mcp-Session-Id`. Trusted axum middleware can instead insert a typed
`LedgerTarget` into the request extensions, which takes precedence over the
header. With neither selector, the request uses `LedgerTarget::default()`
(`startup`).

`register_host_owned` requires the typed `HostOwnedTarget` marker, constructed
from a `NamedLedgerTarget`. The startup registry verifies and rechecks the
bearer's trust root, expiry, and revocation. Only the signed grants travel to
the selected connection. Its policy is checked within the read snapshot or
audited write transaction. Direct grants post normally, propose-only grants
return a pending proposal, and audit entries carry the startup caller's URI
and token ID. The read-only ceiling applies to every target.

Use `register` for a ledger that should authenticate with its own trust root
and bearer. Both registration methods label named tool calls and discovery in
MCP result metadata at `_meta["beankeeper/ledgerTarget"]`. Hosts can deserialize
the value as `NamedLedgerTarget`. The structured result and pagination keep
their existing shapes.

`detach` accepts only a `NamedLedgerTarget`. It stops new requests to that name
with `NOT_FOUND`, waits for requests already admitted, and closes the connection
before returning `DetachedLedger`. Existing sessions can continue using other
targets. A name is single-use for the endpoint's lifetime, so a replacement
ledger needs a fresh name. If removing a file, first release any other host
connections to it and use the file path on the detach proof.

Use startup credentials and select the startup ledger for application host tools.
The configured token fallback also applies there. Endpoints configured with
`LedgerTargets` return HTTP 405 for `Last-Event-ID` because the session cache can
contain responses from different ledgers. This applies even while the registry
has only the startup ledger.
For a lost response to a write, use the tool's existing idempotency mechanism
or check the selected ledger before retrying.

## Tailored demo books

Register `beankeeper_cli::mcp::DemoLedgerTool::new(destination, passphrase)` with
`http_router_with_host_tools` or compose it into your own `HostToolHandler`.
The destination is a fresh path chosen by the host. The optional passphrase
comes from the host's secret storage. The tool is available to callers with
`ledger/admin` on the startup target of a read-write router.

Call `demo_ledger_create` with the profile fields as the tool arguments.
Discovery publishes an inline JSON Schema for
`beankeeper_cli::demo::DemoProfile`. Success returns a plain-language summary,
`is_demo`, and the number of books. Invalid choices and destination collisions
return structured errors. The host can open and register the new database as a
separate ledger target and offer its own Desktop view link.
[Profile example](../README.md#tailor-the-demo-to-your-situation)

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
