//! Sync manifest, two-way sync with conflict copies, and the offline queue.
//!
//! Phase 0: public API placeholder. Implemented in Phase 5.

/// Errors produced by the sync engine.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// A remote operation failed.
    #[error("remote error: {0}")]
    Remote(String),
}
