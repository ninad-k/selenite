//! MCP server exposing the engine to AI agents: tools, resources, prompts and an audit log.
//!
//! Built on the official Rust SDK (`rmcp`). Agents can do everything the UI can do except
//! delete: there is no delete tool, `archive_page` / `archive_task` move items to the vault
//! trash. Every write tool call is recorded in `.selenite/audit/mcp-YYYY-MM.jsonl` (see
//! [`audit`]); the log holds paths, revisions and an argument digest, never content.
//!
//! Transport: stdio ([`serve_stdio`], [`run_stdio`]). Streamable HTTP arrives with the headless
//! server. On stdio, stdout is the protocol channel, so nothing here may print to it; logs go
//! to stderr.

pub mod audit;
mod server;

use rmcp::{ServiceExt, transport::stdio};
use selenite_index::Engine;

pub use audit::{AuditEntry, AuditLog};
pub use server::{
    AgendaParams, AppendBlocksParams, CreateDatabaseParams, CreatePageParams, CreateRowParams,
    CreateTaskParams, DailyNoteParams, FolderParams, ListTasksParams, PathParams,
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
