//! Authenticated HTTP boundary for the shared ledger tool router.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{RoleServer, service::RequestContext};
use secrecy::SecretString;
use serde_json::json;

use super::host::{HostServer, HostToolHandler, NoHostTools, catalog};
use super::targets::OpenedLedger;
use super::{LedgerServer, LedgerTarget, LedgerTargets};
use crate::cli::Cli;
use crate::db::ActorLabel;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::identity::VerifiedAgent;

/// Header selecting a registered ledger for one HTTP MCP request.
pub const LEDGER_TARGET_HEADER: &str = "beankeeper-ledger-target";

/// An opened ledger paired with a bearer verified by its configured authority.
#[derive(Clone)]
pub(super) struct AuthenticatedLedger {
    ledger: OpenedLedger,
    identity: Arc<VerifiedAgent>,
}

impl AuthenticatedLedger {
    fn verify(ledger: OpenedLedger, token: &SecretString) -> Result<Self, CliError> {
        let identity = ledger.verify(token)?;
        Ok(Self {
            ledger,
            identity: Arc::new(identity),
        })
    }

    pub(super) fn ledger(&self) -> &OpenedLedger {
        &self.ledger
    }

    pub(super) fn identity(&self) -> &VerifiedAgent {
        &self.identity
    }
}

pub(super) fn authenticated_ledger(
    context: &RequestContext<RoleServer>,
) -> Result<&AuthenticatedLedger, CliError> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<AuthenticatedLedger>())
        .ok_or_else(|| CliError::Unauthorized("HTTP requires an agent token".into()))
}

#[derive(Clone)]
struct Authentication {
    targets: LedgerTargets,
    fallback: Option<SecretString>,
    allow_event_replay: bool,
}

fn credential(
    request: &mut Request,
    fallback: Option<&SecretString>,
) -> Result<SecretString, CliError> {
    let headers = request.headers_mut();
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let token = match values.next() {
        Some(value) => {
            if values.next().is_some() {
                return Err(missing_credential());
            }
            let (_, token) = value
                .to_str()
                .ok()
                .and_then(|value| value.split_once(' '))
                .filter(|(scheme, token)| {
                    scheme.eq_ignore_ascii_case("Bearer")
                        && !token.is_empty()
                        && !token.bytes().any(|byte| byte.is_ascii_whitespace())
                })
                .ok_or_else(missing_credential)?;
            SecretString::from(token)
        }
        None => fallback.cloned().ok_or_else(missing_credential)?,
    };
    // Keep raw credentials out of rmcp's HTTP headers and diagnostic paths.
    headers.remove(header::AUTHORIZATION);
    Ok(token)
}

fn missing_credential() -> CliError {
    CliError::Unauthorized("HTTP requires a valid Authorization: Bearer agent token".into())
}

fn rejection(error: &CliError) -> Response {
    let status = match error {
        CliError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
        CliError::Forbidden(_) => StatusCode::FORBIDDEN,
        CliError::Validation(_) => StatusCode::BAD_REQUEST,
        CliError::NotFound(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = json!(error.payload()).to_string();
    let mut response = (status, [(header::CONTENT_TYPE, "application/json")], body).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
    }
    response
}

pub(super) fn protocol_error(error: &CliError) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(error.error_code(), Some(json!(error.payload())))
}

async fn authenticate(
    State(state): State<Authentication>,
    mut request: Request,
    next: Next,
) -> Response {
    let target = match request.extensions().get::<LedgerTarget>().map_or_else(
        || LedgerTarget::try_from(request.headers().get_all(LEDGER_TARGET_HEADER)),
        |target| Ok(target.clone()),
    ) {
        Ok(target) => target,
        Err(error) => return rejection(&error),
    };
    let fallback = state.fallback.as_ref().filter(|_| target.named().is_none());
    let token = match credential(&mut request, fallback) {
        Ok(token) => token,
        Err(error) => return rejection(&error),
    };
    let ledger = match state.targets.resolve(&target) {
        Ok(ledger) => ledger,
        Err(error) => return rejection(&error),
    };
    let result =
        tokio::task::spawn_blocking(move || AuthenticatedLedger::verify(ledger, &token)).await;
    match result {
        Ok(Ok(call)) => {
            // rmcp's legacy SSE cache is scoped to the session, not the ledger
            // or bearer that authorized the original POST.
            if !state.allow_event_replay
                && request.method() == Method::GET
                && request.headers().contains_key("last-event-id")
            {
                return StatusCode::METHOD_NOT_ALLOWED.into_response();
            }
            request.extensions_mut().insert(call);
            next.run(request).await
        }
        Ok(Err(error)) => rejection(&error),
        Err(_) => rejection(&CliError::General("HTTP authentication failed".into())),
    }
}

/// Build an authenticated Streamable HTTP router with its endpoint at `/mcp`.
///
/// Merge this router into an axum app to share the host's listener and runtime.
/// Only `/mcp` is authenticated by this router; sibling routes keep their own
/// middleware. No listener, runtime, signal handler, or background task is started.
/// Construction performs blocking passphrase resolution and database I/O, so call
/// it during startup or from `tokio::task::spawn_blocking` in an async host.
///
/// The ledger must already exist. The passphrase is resolved once from `cli`,
/// without prompting. Every MCP request verifies a bearer credential, even when
/// ledger capability enforcement is off. `cli.agent_token` is a fallback only
/// for startup requests when the Authorization header is absent. Additional
/// CLI `--ledger` bindings are opened with the same passphrase. Tool calls
/// recheck identity state and current policy atomically with execution.
/// `read_only` always removes mutation tools.
///
/// The host supplies the transport configuration, including Host/Origin rules.
/// Clone `config.cancellation_token` before passing it in, then cancel it during
/// host shutdown to terminate MCP sessions. The host also owns listener shutdown.
/// CLI bind flags are not used by this factory.
/// Hosts that already resolved a secret should use [`http_router_with_passphrase`]
/// to share it with other connections without consuming its source again.
///
/// # Example
///
/// ```no_run
/// use axum::{Router, routing::get};
/// use beankeeper_cli::{
///     cli::Cli,
///     mcp::{http_router, StreamableHttpServerConfig},
/// };
/// use clap::Parser;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let cli = Cli::parse_from(["host", "--db", "/path/books.db", "mcp", "serve"]);
/// let config = StreamableHttpServerConfig::default().enforce_origin_validation();
/// let cancellation = config.cancellation_token.clone();
/// // http_router already mounts /mcp; merge it beside the host's routes.
/// let app = Router::new()
///     .route("/health", get(|| async { "ok" }))
///     .merge(http_router(&cli, false, config)?);
///
/// tokio::runtime::Runtime::new()?.block_on(async move {
///     let listener = tokio::net::TcpListener::bind("127.0.0.1:8750").await?;
///     axum::serve(listener, app)
///         .with_graceful_shutdown(async move {
///             let _ = tokio::signal::ctrl_c().await;
///             cancellation.cancel();
///         })
///         .await
/// })?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns a CLI error if passphrase resolution or opening the ledger fails.
pub fn http_router(
    cli: &Cli,
    read_only: bool,
    config: StreamableHttpServerConfig,
) -> Result<Router, CliError> {
    let passphrase = crate::passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    http_router_with_passphrase(cli, passphrase.as_ref(), read_only, config)
}

/// Build an authenticated `/mcp` router using an already-resolved passphrase.
///
/// This has the same authentication, capability, read-only, transport and host
/// lifecycle behavior as [`http_router`]. It opens the existing ledger at
/// `cli.db` with `passphrase`, ignoring CLI passphrase sources and
/// `BEANKEEPER_PASSPHRASE_CMD`. It never prompts or creates a passphrase file.
/// Pass `None` for an unencrypted ledger.
///
/// The secret is borrowed only during construction and is not retained by the
/// router. Hosts can reuse it for [`Db::open_snapshot`] and drop it after opening
/// both connections. This API is platform-independent and needs no file
/// descriptor bridge. Construction performs blocking database I/O.
///
/// # Example
///
/// ```no_run
/// use beankeeper_cli::{
///     cli::Cli,
///     db::Db,
///     mcp::{http_router_with_passphrase, StreamableHttpServerConfig},
///     passphrase::resolve_passphrase,
/// };
/// use clap::Parser;
///
/// # fn main() -> Result<(), beankeeper_cli::error::CliError> {
/// let cli = Cli::parse_from(["host", "--db", "/path/books.db", "mcp", "serve"]);
/// let passphrase = resolve_passphrase(
///     cli.passphrase.passphrase_fd,
///     cli.passphrase.passphrase_file.as_deref(),
///     false,
/// )?;
/// let config = StreamableHttpServerConfig::default().enforce_origin_validation();
/// let app = http_router_with_passphrase(&cli, passphrase.as_ref(), false, config)?;
/// let snapshot = Db::open_snapshot(&cli.db, passphrase.as_ref())?;
/// drop(passphrase);
/// // Merge app into the host router and use snapshot for read-only queries.
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns a CLI error if opening the existing ledger with the supplied key fails.
pub fn http_router_with_passphrase(
    cli: &Cli,
    passphrase: Option<&SecretString>,
    read_only: bool,
    config: StreamableHttpServerConfig,
) -> Result<Router, CliError> {
    http_router_with_host_tools(cli, passphrase, read_only, config, NoHostTools)
}

/// Compose host and ledger MCP tools behind the authenticated `/mcp` boundary.
///
/// This shares the resolved-passphrase and lifecycle contract of
/// [`http_router_with_passphrase`]. The host's catalog is captured once; duplicate
/// names, including collisions with built-in tools, fail construction. Read-only
/// mode removes every host tool declared as a mutation, including navigation.
///
/// Discovery and dispatch require each host tool's explicit signed grant even
/// when ledger enforcement is off. Dispatch supplies a freshly verified,
/// request-scoped identity to [`HostToolHandler::call_tool`]. Header credentials
/// override the CLI fallback; sessions do not retain credentials. Ledger tools
/// retain their existing live policy checks and atomic execution.
///
/// See the [host integration guide](https://github.com/Govcraft/beankeeper/blob/main/docs/mcp-host-tools.md)
/// for a complete host example and grant contract.
///
/// # Errors
///
/// Returns a CLI error for an invalid catalog or failure to open the ledger.
pub fn http_router_with_host_tools<H: HostToolHandler>(
    cli: &Cli,
    passphrase: Option<&SecretString>,
    read_only: bool,
    config: StreamableHttpServerConfig,
    host: H,
) -> Result<Router, CliError> {
    let server = host_server(cli, read_only, host)?;
    let paths = cli.ledger_paths()?;
    let targets = LedgerTargets::new(Db::open_existing(&cli.db, passphrase)?);
    for (target, path) in paths {
        targets.register(target.clone().into(), Db::open_existing(path, passphrase)?)?;
    }
    Ok(build_router(cli, targets, config, server, paths.is_empty()))
}

/// Serve already-opened ledgers and application tools on one authenticated HTTP MCP endpoint.
///
/// Keep a clone of `targets` to register or detach ledgers at runtime.
/// Clients select a ledger with [`LEDGER_TARGET_HEADER`] on each request, including
/// discovery. A host can insert a typed [`LedgerTarget`] in axum request
/// extensions to override header selection. With neither selector, the router
/// uses the startup ledger. Supply a bearer issued by the selected ledger, or
/// by the startup ledger for a target registered as host-owned.
/// The CLI token fallback and application host tools are available on the
/// startup ledger. A single MCP session can send calls to
/// different registered ledgers with their respective credentials.
/// SSE replay via `Last-Event-ID` is disabled because the transport cache is
/// scoped to the session and could replay a response from a different ledger.
///
/// This factory reads the actor and fallback token from `cli`. It uses the
/// connections supplied in `targets` and starts no listener or runtime.
///
/// # Errors
/// Returns a CLI error for an invalid host tool catalog.
pub fn http_router_with_ledger_targets<H: HostToolHandler>(
    cli: &Cli,
    targets: LedgerTargets,
    read_only: bool,
    config: StreamableHttpServerConfig,
    host: H,
) -> Result<Router, CliError> {
    let server = host_server(cli, read_only, host)?;
    Ok(build_router(cli, targets, config, server, false))
}

fn host_server<H: HostToolHandler>(
    cli: &Cli,
    read_only: bool,
    host: H,
) -> Result<HostServer<H>, CliError> {
    Ok(HostServer {
        tools: catalog(host.tools(), read_only)?,
        ledger: LedgerServer::with_backend(
            read_only.into(),
            super::Backend::Http,
            ActorLabel::configured(cli.actor.as_deref()),
        ),
        host: Arc::new(host),
    })
}

fn build_router<H: HostToolHandler>(
    cli: &Cli,
    targets: LedgerTargets,
    config: StreamableHttpServerConfig,
    server: HostServer<H>,
    allow_event_replay: bool,
) -> Router {
    let authentication = Authentication {
        targets,
        fallback: cli.agent_token.clone(),
        allow_event_replay,
    };
    let service = StreamableHttpService::new(
        move || {
            Ok(HostServer {
                ledger: server.ledger.clone(),
                host: server.host.clone(),
                tools: server.tools.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(authentication, authenticate))
}

/// Serve the ledger at `/mcp` using authenticated Streamable HTTP.
///
/// Wraps [`http_router`] with a runtime, listener and Ctrl-C shutdown. Every
/// request verifies a bearer credential, including when capability enforcement
/// is off. The passphrase is resolved once, without prompting.
///
/// # Errors
///
/// Returns a CLI error for an unsafe bind, startup, database or transport failure.
pub fn serve_http(
    cli: &Cli,
    read_only: bool,
    bind: SocketAddr,
    allow_remote: bool,
) -> Result<(), CliError> {
    if !bind.ip().is_loopback() && !allow_remote {
        return Err(CliError::Usage(
            "non-loopback HTTP binds require --allow-remote (plaintext HTTP, no TLS)".into(),
        ));
    }
    let mut config = StreamableHttpServerConfig::default().enforce_origin_validation();
    if bind.ip().is_unspecified() {
        // An explicitly enabled remote wildcard bind has no fixed Host.
        config = config.disable_allowed_hosts();
    } else {
        config.allowed_hosts.push(bind.ip().to_string());
    }
    let cancellation = config.cancellation_token.clone();
    let app = http_router(cli, read_only, config)?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(async move {
            let listener = tokio::net::TcpListener::bind(bind).await?;
            let bound = listener.local_addr()?;
            eprintln!("MCP Streamable HTTP listening on http://{bound}/mcp");
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    cancellation.cancel();
                })
                .await;
            result.map_err(CliError::from)
        })
}
