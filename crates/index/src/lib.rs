//! Disposable SQLite cache of pages, links, backlinks, tags, properties and full-text search,
//! plus the [`Engine`] facade shared by the CLI, desktop app and MCP server.
//!
//! The index is never synced and is always rebuildable from the vault files
//! (`Engine::rebuild_index`).

pub mod ask;
pub mod blocks;
pub mod databases;
mod db;
pub mod engine;
pub mod passages;
pub mod query;
mod schema;
pub mod tasks;

pub use ask::Citation;
pub use blocks::{BlockEdit, BlockPage, RenderedBlock};
pub use databases::{DatabaseInfo, Row, ViewGroup, ViewResult};
pub use db::{
    Backlink, HIGHLIGHT_END, HIGHLIGHT_START, Index, LinkRecord, OpenStatus, OutLink, PageRecord,
    PageSummary, SearchHit, TagCount, TaskRecord, TaskRow, UnresolvedLink,
};
pub use engine::{Engine, EngineError, TreeNode};
pub use passages::{Passage, split_passages};
pub use query::SearchQuery;
pub use tasks::{Agenda, AgendaDay, ProjectCount, TaskItem};

/// Schema version of the index, stored in `PRAGMA user_version`. A mismatch rebuilds the index.
///
/// History: 1 = pages, aliases, links, tags, properties, fts (Phase 1); 2 = tasks (Phase 2);
/// 3 = wikilinks in front matter values are indexed as links (Phase 4);
/// 4 = `passages` FTS5 table for cited retrieval (Phase 7).
pub const INDEX_VERSION: u32 = 4;

/// File name of the index inside the vault's app folder.
pub const INDEX_FILE: &str = "index.sqlite";

/// Errors produced by the index.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The underlying database failed.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    /// A filesystem operation on the index file failed.
    #[error("index file error: {0}")]
    Io(String),
}

/// Result alias for index operations.
pub type Result<T> = std::result::Result<T, IndexError>;
