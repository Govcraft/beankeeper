//! Authenticated HTTP boundary for the shared ledger tool router.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;

use super::LedgerServer;
use crate::cli::Cli;
use crate::db::connection::Db;
use crate::error::CliError;

/// Credential attached to this HTTP request, never to its MCP session.
#[derive(Clone)]
pub(super) struct RequestToken(pub SecretString);

#[derive(Clone)]
struct Authentication {
    db: Arc<Mutex<Db>>,
    fallback: Option<SecretString>,
}

fn credential(
    request: &mut Request,
    fallback: Option<&SecretString>,
) -> Result<RequestToken, CliError> {
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
    Ok(RequestToken(token))
}

fn missing_credential() -> CliError {
    CliError::Unauthorized("HTTP requires a valid Authorization: Bearer agent token".into())
}

fn rejection(error: &CliError) -> Response {
    let status = match error {
        CliError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
        CliError::Forbidden(_) => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = json!({"code": error.error_code(), "message": error.to_string()}).to_string();
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
    rmcp::ErrorData::internal_error(
        error.error_code(),
        Some(json!({"code": error.error_code(), "message": error.to_string()})),
    )
}

async fn authenticate(
    State(state): State<Authentication>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = match credential(&mut request, state.fallback.as_ref()) {
        Ok(token) => token,
        Err(error) => return rejection(&error),
    };
    let verify_token = token.clone();
    let result = tokio::task::spawn_blocking(move || {
        let db = state
            .db
            .lock()
            .map_err(|_| CliError::Database("ledger connection is unavailable".into()))?;
        crate::identity::verify(db.conn(), verify_token.0.expose_secret()).map(|_| ())
    })
    .await;
    match result {
        Ok(Ok(())) => {
            request.extensions_mut().insert(token);
            next.run(request).await
        }
        Ok(Err(error)) => rejection(&error),
        Err(_) => rejection(&CliError::General("HTTP authentication failed".into())),
    }
}

/// Serve the ledger at `/mcp` using authenticated Streamable HTTP.
///
/// Every request verifies a bearer credential, including when ledger capability
/// enforcement is off. An explicit Authorization header overrides the optional
/// process credential. Tool calls recheck credentials and policy atomically with
/// execution. The passphrase is resolved once, without prompting.
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
    let passphrase = crate::passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open_existing(&cli.db, passphrase.as_ref())?;
    drop(passphrase);
    let mut server = LedgerServer::new(db, read_only);
    server.require_request_token = true;
    let authentication = Authentication {
        db: server.db.clone(),
        fallback: cli.agent_token.clone(),
    };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(async move {
            let listener = tokio::net::TcpListener::bind(bind).await?;
            let bound = listener.local_addr()?;
            let mut config = StreamableHttpServerConfig::default().enforce_origin_validation();
            if bind.ip().is_unspecified() {
                // An explicitly enabled remote wildcard bind has no fixed Host.
                config = config.disable_allowed_hosts();
            } else {
                config.allowed_hosts.push(bind.ip().to_string());
            }
            let cancellation = config.cancellation_token.clone();
            let service = StreamableHttpService::new(
                move || Ok(server.clone()),
                Arc::new(LocalSessionManager::default()),
                config,
            );
            let app = axum::Router::new()
                .nest_service("/mcp", service)
                .layer(middleware::from_fn_with_state(authentication, authenticate));
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
