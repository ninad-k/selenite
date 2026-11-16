//! The `StorageProvider` abstraction over user-owned storage.
//!
//! Phase 0: public API placeholder. `LocalFolder` lands in Phase 1, cloud drives in Phase 5.

/// Errors returned by storage providers.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The requested file does not exist.
    #[error("not found: {0}")]
    NotFound(String),
}
