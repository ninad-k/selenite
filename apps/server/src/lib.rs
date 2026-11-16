//! Headless server: a vault served over MCP Streamable HTTP, with optional background sync.
//!
//! [`run`] starts everything on its own thread and runtime and returns the bound address and a
//! [`ServerHandle`], so tests (and the binary) do not need the process to be the server.
//!
//! Security model: every request to `/mcp` needs `Authorization: Bearer <token>` (constant-time
//! check, see [`auth`]) and, if it carries an `Origin`, that origin must be loopback or
//! explicitly allowed. `/healthz` is public and says nothing about the vault. The server speaks
//! plain HTTP: put a TLS-terminating reverse proxy in front of it for anything but loopback.

pub mod auth;
pub mod sync_loop;

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use selenite_index::Engine;
use selenite_mcp::{HttpConfig, VaultMcp, streamable_http_service};
use tokio::sync::watch;

use crate::auth::{OriginPolicy, TokenGuard};

/// Errors starting or running the server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The vault could not be opened.
    #[error("cannot open vault: {0}")]
    Vault(String),
    /// The token could not be read or created.
    #[error("token error: {0}")]
    Token(String),
    /// The listen address could not be bound or the runtime could not start.
    #[error("cannot start server: {0}")]
    Start(String),
}

/// Everything the server needs.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Vault folder.
    pub vault: PathBuf,
    /// Address to listen on (use port 0 to let the OS pick one).
    pub listen: SocketAddr,
    /// Bearer token clients must present.
    pub token: String,
    /// Background sync interval; `None` disables sync.
    pub sync_interval: Option<Duration>,
    /// Extra allowed `Origin` values (loopback origins are always allowed).
    pub allowed_origins: Vec<String>,
    /// Extra allowed `Host` values. When empty and `listen` is not loopback, host checking
    /// is off (the token is the gate); otherwise loopback hosts plus these are accepted.
    pub allowed_hosts: Vec<String>,
}

/// Stops the server when shut down or dropped.
pub struct ServerHandle {
    cancel: Box<dyn Fn() + Send + Sync>,
    stop_sync: watch::Sender<bool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ServerHandle {
    /// Stop accepting requests, end open sessions and wait for the server thread to finish.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        (self.cancel)();
        let _ = self.stop_sync.send(true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Gate {
    token: TokenGuard,
    origins: OriginPolicy,
}

fn status(code: StatusCode) -> Response {
    code.into_response()
}

/// Origin check first (403), then the bearer token (401). Responses carry no detail.
async fn gate(State(gate): State<Arc<Gate>>, req: Request, next: Next) -> Response {
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let allowed = origin.to_str().is_ok_and(|o| gate.origins.permits(o));
        if !allowed {
            tracing::warn!("rejected request: origin not allowed");
            return status(StatusCode::FORBIDDEN);
        }
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if !gate.token.check(presented) {
        tracing::warn!("rejected request: missing or invalid token");
        let mut resp = status(StatusCode::UNAUTHORIZED);
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
        return resp;
    }
    next.run(req).await
}

/// Build the HTTP application: public `/healthz`, authenticated `/mcp`.
fn router(vault: &VaultMcp, config: &ServerConfig, http: HttpConfig) -> Router {
    let gate_state = Arc::new(Gate {
        token: TokenGuard::new(&config.token),
        origins: OriginPolicy::new(&config.allowed_origins),
    });
    let mcp = Router::new()
        .route_service("/mcp", streamable_http_service(vault, http))
        .layer(middleware::from_fn_with_state(gate_state, gate));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(mcp)
}

/// Start the server. Returns once the socket is bound; the server runs on a background thread
/// until the handle is shut down or dropped.
pub fn run(config: ServerConfig) -> Result<(SocketAddr, ServerHandle), ServerError> {
    let engine =
        Engine::open_local(&config.vault).map_err(|e| ServerError::Vault(e.to_string()))?;
    let vault = VaultMcp::new(engine);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ServerError::Start(e.to_string()))?;
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(config.listen))
        .map_err(|e| ServerError::Start(format!("{}: {e}", config.listen)))?;
    let addr = listener
        .local_addr()
        .map_err(|e| ServerError::Start(e.to_string()))?;

    let mut http = HttpConfig::default();
    if config.allowed_hosts.is_empty() {
        if !config.listen.ip().is_loopback() {
            http = http.disable_allowed_hosts();
        }
    } else {
        let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"]
            .map(String::from)
            .into_iter()
            .collect();
        hosts.extend(config.allowed_hosts.iter().cloned());
        http = http.with_allowed_hosts(hosts);
    }
    let ct = http.cancellation_token.clone();
    let app = router(&vault, &config, http);

    let (stop_tx, stop_rx) = watch::channel(false);
    let sync_task = config.sync_interval.map(|interval| {
        runtime.spawn(sync_loop::run_loop(
            config.vault.clone(),
            vault.clone(),
            interval,
            stop_rx,
        ))
    });

    let shutdown = ct.clone();
    let thread = std::thread::Builder::new()
        .name("selenite-server".into())
        .spawn(move || {
            let serve = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.clone().cancelled_owned());
            let result = runtime.block_on(async {
                tokio::select! {
                    r = serve.into_future() => r,
                    // Graceful shutdown waits for open streams; do not wait forever.
                    () = async {
                        shutdown.cancelled().await;
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    } => Ok(()),
                }
            });
            if let Err(e) = result {
                tracing::error!(error = %e, "server stopped with an error");
            }
            if let Some(task) = sync_task {
                task.abort();
            }
            runtime.shutdown_timeout(Duration::from_secs(5));
        })
        .map_err(|e| ServerError::Start(e.to_string()))?;

    tracing::info!(%addr, "listening");
    Ok((
        addr,
        ServerHandle {
            cancel: Box::new(move || ct.cancel()),
            stop_sync: stop_tx,
            thread: Some(thread),
        },
    ))
}
