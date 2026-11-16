//! Two-way sync between a local vault and a remote `StorageProvider`.
//!
//! Files are the truth (ADR-002) and the index is never synced (ADR-003). A per-remote
//! [`Manifest`] records the content hash and revisions seen at the last sync; each path is then
//! compared three ways (local, remote, manifest). The core safety rule: a file that changed on
//! the other side since the last sync is **never overwritten**. Both versions are kept: the
//! remote version is written next to the local one as a conflict copy and uploaded too.
//! Deletes go through `StorageProvider::trash`, never a hard delete. Nothing here logs file
//! content, only paths and counts.

mod engine;
mod manifest;
mod queue;

pub use engine::{ConflictInfo, SyncReport, sync};
pub use manifest::{ConflictState, Manifest, ManifestEntry, is_synced};
pub use queue::{OfflineQueue, is_offline, pending};

use selenite_storage::StorageError;

/// Errors that abort a whole sync run. Per-path failures are reported in
/// [`SyncReport::errors`] instead.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// Listing the remote failed for a reason other than being offline.
    #[error("remote error: {0}")]
    Remote(String),
    /// A local storage operation failed (listing, manifest or queue persistence).
    #[error("local storage error: {0}")]
    Storage(#[from] StorageError),
    /// The manifest or queue file could not be parsed or serialised.
    #[error("sync state error: {0}")]
    State(String),
    /// The remote identifier cannot be used to name the manifest file.
    #[error("invalid remote id: {0}")]
    InvalidRemoteId(String),
}
