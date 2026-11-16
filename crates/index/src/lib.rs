//! Disposable SQLite cache of links, backlinks, tags, properties, tasks and full-text search.
//!
//! The index is never synced and is always rebuildable from the vault files.
//! Phase 0: public API placeholder. Implemented in Phase 1.

/// Schema version of the index. A mismatch triggers a full rebuild.
pub const INDEX_VERSION: u32 = 0;

/// Errors produced by the index.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The underlying database failed.
    #[error("database error: {0}")]
    Db(String),
}
