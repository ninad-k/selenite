//! MCP server exposing the engine to AI agents: tools, resources, prompts and an audit log.
//!
//! Built on the official Rust SDK (`rmcp`). Agents can do everything the UI can do except
//! delete: there is no delete tool, `archive_page` / `archive_task` move items to the vault
//! trash. Every write tool call is recorded in `.selenite/audit/mcp-YYYY-MM.jsonl` (see
//! [`audit`]); the log holds paths, revisions and an argument digest, never content.
//!
//! Transports: stdio ([`serve_stdio`], [`run_stdio`]) and Streamable HTTP
//! ([`streamable_http_service`], mounted and authenticated by the headless `selenite-server`).
//! Audit entries record which one a call used. On stdio, stdout is the protocol channel, so nothing here may print to it; logs go
//! to stderr.

pub use selenite_index::audit;
mod server;

use std::sync::Arc;

use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use selenite_index::Engine;

pub use audit::{AuditEntry, AuditLog};
pub use server::{
    AgendaParams, AppendBlocksParams, AskParams, CreateDatabaseParams, CreatePageParams,
    CreateRowParams, CreateTaskParams, DailyNoteParams, FolderParams, ListTasksParams, PathParams,
    QueryDatabaseParams, SearchParams, SetReminderParams, SetRowPropertyParams, UpdateBlockParams,
    UpdateDatabaseSchemaParams, UpdateTaskParams, VaultMcp,
};

#[cfg(test)]
mod tests;

/// Errors produced by the MCP layer.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// A tool call failed.
    #[error("tool error: {0}")]
    Tool(String),
    /// The transport or protocol handshake failed.
    #[error("MCP transport error: {0}")]
    Transport(String),
    /// The async runtime could not be started.
    #[error("could not start the async runtime: {0}")]
    Runtime(String),
}

/// Serve `engine` over stdio until the client disconnects.
pub async fn serve_stdio(engine: Engine) -> Result<(), McpError> {
    let service = VaultMcp::new(engine)
        .serve(stdio())
        .await
        .map_err(|e| McpError::Transport(e.to_string()))?;
    service
        .waiting()
        .await
        .map_err(|e| McpError::Transport(e.to_string()))?;
    Ok(())
}

/// Blocking wrapper around [`serve_stdio`]: builds a multi-thread tokio runtime and runs it.
pub fn run_stdio(engine: Engine) -> Result<(), McpError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| McpError::Runtime(e.to_string()))?;
    runtime.block_on(serve_stdio(engine))
}

/// The Streamable HTTP service for `vault`: a `tower` service to mount at `/mcp`.
///
/// Every session gets a clone of `vault` (so one engine, serialised by its lock, serves all
/// sessions) tagged with the `http` audit transport. The service itself does no
/// authentication: the caller must put authentication in front of it.
pub fn streamable_http_service(
    vault: &VaultMcp,
    config: StreamableHttpServerConfig,
) -> StreamableHttpService<VaultMcp, LocalSessionManager> {
    let vault = vault.clone().with_transport("http");
    StreamableHttpService::new(
        move || Ok(vault.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

/// Re-exported so callers can name the HTTP configuration without depending on `rmcp`.
pub use rmcp::transport::streamable_http_server::StreamableHttpServerConfig as HttpConfig;
