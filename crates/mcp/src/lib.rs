//! MCP server exposing the engine to AI agents: tools, resources, prompts and an audit log.
//!
//! Phase 0: public API placeholder. Implemented in Phase 3.

/// Errors produced by the MCP layer.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// A tool call failed.
    #[error("tool error: {0}")]
    Tool(String),
}
